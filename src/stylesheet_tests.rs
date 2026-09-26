//! The staleness guard for the committed stylesheet
//! (`odd/tasks/tailwind-stylesheet-rebuild.md`, work unit U1).
//!
//! `static/tailwind.css` is generated and committed, which makes it a promise:
//! a clean checkout with no Tailwind CLI installed must still render every
//! class the templates use. The file went stale silently once already — fifteen
//! utility classes the templates had been using for weeks had no rule at all,
//! and nothing failed. This module is the thing that would have failed.
//!
//! # Why the parsing here is not naive
//!
//! Tailwind v4 scans the *raw text* of a source file, comments included, so a
//! naive word-splitting reader and the compiler disagree about what a class is
//! in both directions. On this tree, splitting every scanned file on whitespace
//! yields **thousands** of class-shaped tokens that are not in the compiled
//! stylesheet, against **15** real ones. The exact figure is not recorded here
//! on purpose: it is entirely a function of the filter chosen for "looks like a
//! class", and a number nobody can reproduce with a command is a number nobody
//! should rely on. A guard that reported those thousands would be noise, and a
//! guard written to hide them would hide the real 15 too. So the extraction is
//! anchored, not lexical. The three behaviours that make the difference are each
//! pinned by a test, so `cargo test stylesheet` reproduces the claim:
//!
//! * **Comment prose.** A bare word that is also a utility (`blur`, `inline`)
//!   appearing in a comment once produced unused `.blur`/`.inline` rules in this
//!   very repository. Comments are stripped on both sides of the comparison.
//! * **Askama control payloads.** `{% if %}` payloads are dropped, because `if`,
//!   `else`, `endif` and an expression like
//!   `product.kind.to_string() == "Service"` are not classes. The literal text
//!   on *both* sides of a control block is kept, because either branch can
//!   render and both need rules.
//! * **JavaScript boolean arguments.** `classList.toggle('htmx-request', status
//!   === 'searching')` must yield `htmx-request` and not `searching`, while
//!   `classList.toggle('htmx-request')` must still yield `htmx-request`.
//!
//! * Templates contribute only `class="…"` attribute **values**, and only the
//!   literal text between Askama control blocks.
//! * Askama blocks are skipped **atomically** while reading the attribute
//!   value. Nine class attributes in this tree contain a raw `"` inside a
//!   `{% if x == "Service" %}`; a `[^"]*` attribute reader terminates on that
//!   quote and emits the truncated control payload as class names.
//! * JavaScript sources contribute only `className = "…"` assignments and the
//!   string arguments of `classList.add/remove/toggle(…)`. `static/picker.js`
//!   opens with a 37-line block comment full of class-shaped words
//!   (`htmx-request`, `htmx-indicator`, `state`, `query`, …); anchoring is what
//!   keeps prose out, independently of comment stripping.
//! * Comments are stripped on both sides anyway, so a future template that
//!   documents a class in a `{# … #}` or `<!-- … -->` block cannot invent a
//!   false positive later.
//!
//! # Why the lookup is escape-aware
//!
//! The lookup is not a substring search. `py-2.5` is compiled to `.py-2\.5`,
//! `sm:grid-cols-2` to `.sm\:grid-cols-2`, `min-h-[70vh]` to
//! `.min-h-\[70vh\]` and `sm:grid-cols-[100px_1fr_120px_auto]` to
//! `.sm\:grid-cols-\[100px_1fr_120px_auto\]`; a naive grep finds none of them,
//! which is the false-positive trap `odd/tasks/cost-price-freshness.md` and
//! `odd/tasks/filter-honest-product-mutations.md` both record. Instead the
//! compiled stylesheet is *parsed* — every class selector is extracted from the
//! rule prelude and unescaped per the CSS escape rules — and a token is present
//! when its unescaped name is in that set. Set membership on unescaped names is
//! exact: it cannot be fooled by a token that is a substring of another class
//! (`text-sm` inside `sm:text-sm`), and it needs no hand-written escaping table
//! to stay correct across Tailwind versions.
//!
//! # Known limit, accepted on purpose
//!
//! The guard reads class **attributes** plus the scanned JavaScript files. It
//! does not read `className`/`classList` inside inline `<script>` blocks, and
//! this tree has **27** such sites: three in `base.html` and four each in
//! `customers`, `documents`, `products`, `purchase`, `purchases` and
//! `suppliers`. Together they write **six** distinct class names —
//! `-translate-x-full`, `flex`, `hidden`, `notice`, `notice-error` and
//! `notice-success` — and all six have rules in the compiled stylesheet.
//!
//! The direction of this blindness is safe, and that is the reason it is
//! accepted rather than closed. Tailwind scans those script bodies as raw text,
//! so the compiler's coverage there is a **superset** of the guard's: the guard
//! can under-report, never false-positive, and it can never hide a class the
//! compiler successfully compiled. Catching one would need the two to disagree
//! about the same bytes. What the limit actually costs is scope: the guard's
//! coverage claim is "class attributes plus the JS scan sources", and any claim
//! wider than that is false.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// The Tailwind entrypoint. It is the authority on *what is scanned*: the
/// guard reads its `@source` declarations instead of hardcoding a template
/// path, so adding a scan source cannot silently fall outside the net.
const ENTRYPOINT: &str = "assets/tailwind.css";

/// The generated stylesheet that is committed and served. The guard reads this
/// file only — never a rebuild — because the promise it protects is "a clean
/// checkout renders", not "the build is reproducible".
const COMPILED: &str = "static/tailwind.css";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Read a repository file, failing with its path rather than a bare IO error.
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// Lexically resolve `.` and `..` so a declared `../templates` is reported as
/// `templates/…` instead of `assets/../templates/…`.
fn normalize(path: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str().to_os_string()),
        }
    }
    out.iter().collect()
}

/// Remove `/* … */` comments. CSS comments do not nest, and the compiled file
/// opens with a licence banner whose `tailwindcss.com` and `v4.3.3` would
/// otherwise each be collected as a class selector.
fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(open) = rest.find("/*") {
        out.push_str(&rest[..open]);
        match rest[open + 2..].find("*/") {
            Some(close) => rest = &rest[open + 2 + close + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The `@source` paths declared by the entrypoint, resolved against the
/// directory that declares them. The `@import … source(none)` that disables
/// auto-detection is why these declarations are the whole scan surface.
fn declared_scan_sources(entrypoint: &Path) -> Vec<PathBuf> {
    let text = strip_css_comments(&read(entrypoint));
    let dir = entrypoint.parent().unwrap_or_else(|| Path::new("."));
    let mut sources = Vec::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("@source") {
        let after = rest[at + "@source".len()..].trim_start();
        assert!(
            after.starts_with('"'),
            "unsupported @source form in {}: {:?}. This guard understands the \
             plain quoted-path form only; teach it the new form rather than \
             letting the new source fall outside the net.",
            entrypoint.display(),
            after.chars().take(40).collect::<String>()
        );
        let quoted = &after[1..];
        let close = quoted
            .find('"')
            .unwrap_or_else(|| panic!("unterminated @source path in {}", entrypoint.display()));
        sources.push(normalize(&dir.join(&quoted[..close])));
        rest = &quoted[close + 1..];
    }
    assert!(
        !sources.is_empty(),
        "{} declares no @source, so the guard would check nothing. The \
         entrypoint imports Tailwind with source(none): without a @source the \
         compiled stylesheet is empty and the guard is vacuous.",
        entrypoint.display()
    );
    sources
}

/// Every file a `@source` path contributes: the path itself when it names a
/// file, or a recursive walk when it names a directory.
fn source_files(source: &Path) -> Vec<PathBuf> {
    if source.is_file() {
        return vec![source.to_path_buf()];
    }
    assert!(
        source.is_dir(),
        "{} is neither a file nor a directory; a @source that points nowhere \
         makes the guard blind to whatever it was meant to scan",
        source.display()
    );
    let mut files = Vec::new();
    let mut stack = vec![source.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()))
            .map(|entry| entry.expect("directory entry").path())
            .collect();
        // Sorted so a failure message lists files in a stable order.
        entries.sort();
        for path in entries {
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    assert!(
        !files.is_empty(),
        "{} yielded no files; the guard would check nothing",
        source.display()
    );
    files
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Class tokens contributed by one scanned file, dispatched by extension.
fn class_tokens(path: &Path) -> Vec<String> {
    let text = read(path);
    match extension(path).as_str() {
        "html" => {
            html_class_tokens(&text).unwrap_or_else(|err| panic!("{err} (in {})", path.display()))
        }
        "js" => js_class_tokens(&text),
        other => panic!(
            "no class extractor for a .{other} file at {}. Add one, or the \
             classes that live there are outside the guard's net.",
            path.display()
        ),
    }
}

// -----------------------------------------------------------------------------
// HTML: `class="…"` attribute values, Askama-aware
// -----------------------------------------------------------------------------

/// True for a byte that can be part of an attribute name, so `data-class="…"`
/// is not mistaken for `class="…"`.
fn is_attribute_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':' | b'@' | b'.')
}

/// The raw value of every `class="…"` attribute, paired with the byte offset of
/// the attribute so a failure can name the file and the position.
fn class_attribute_values(text: &str) -> Vec<(&str, usize)> {
    let bytes = text.as_bytes();
    let mut values = Vec::new();
    let mut at = 0;
    while let Some(found) = text[at..].find("class") {
        let name = at + found;
        at = name + "class".len();
        if name > 0 && is_attribute_name_byte(bytes[name - 1]) {
            continue;
        }
        let mut cursor = at;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'"' {
            continue;
        }
        let start = cursor + 1;
        let mut end = start;
        while end < bytes.len() {
            // Skip an Askama block whole. A `{% if x == "Service" %}` inside a
            // class attribute carries raw quotes, and treating one as the end
            // of the value truncates the attribute and leaks the control
            // payload into the token list.
            if text[end..].starts_with("{%") || text[end..].starts_with("{#") {
                let close = if text[end..].starts_with("{%") {
                    "%}"
                } else {
                    "#}"
                };
                match text[end..].find(close) {
                    Some(offset) => {
                        end += offset + close.len();
                        continue;
                    }
                    None => {
                        end = bytes.len();
                        break;
                    }
                }
            }
            if bytes[end] == b'"' {
                break;
            }
            end += 1;
        }
        values.push((&text[start..end.min(bytes.len())], name));
        at = end.min(bytes.len());
    }
    values
}

/// Replace every Askama control block and comment span with a space.
///
/// The control block is dropped whole — `if`, `else`, `endif`, `for` and any
/// expression inside are syntax, not classes. Dropping it and keeping the
/// literal text around it yields the union of the branches that can render,
/// which is exactly the set that needs a rule.
fn strip_askama_blocks(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let control = rest.find("{%").map(|at| (at, "%}", 2));
        let comment = rest.find("{#").map(|at| (at, "#}", 2));
        let next = match (control, comment) {
            (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let Some((at, close, open_len)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        out.push(' ');
        match rest[at + open_len..].find(close) {
            Some(offset) => rest = &rest[at + open_len + offset + close.len()..],
            None => return out,
        }
    }
}

/// Remove HTML (`<!-- … -->`) and Askama (`{# … #}`) comment spans.
///
/// A template that documents a class in a comment is the ordinary case, and
/// Tailwind's own scanner compiles it — so a comment is a place a false
/// positive comes from. None of this tree's 1427 class attributes sits inside a
/// comment today, which is exactly why stripping has to be a property of the
/// extractor rather than of the current markup.
fn strip_markup_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let html = rest.find("<!--").map(|at| (at, "-->"));
        let askama = rest.find("{#").map(|at| (at, "#}"));
        let next = match (html, askama) {
            (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let Some((at, close)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        out.push(' ');
        match rest[at..].find(close) {
            Some(offset) => rest = &rest[at + offset + close.len()..],
            None => return out,
        }
    }
}

/// Class tokens from an Askama template.
///
/// Fails loudly on `{{ … }}` inside a class attribute: the guard cannot know
/// which classes an interpolated value renders, so quietly skipping it would
/// turn a blind spot into a passing test.
fn html_class_tokens(text: &str) -> Result<Vec<String>, String> {
    let text = strip_markup_comments(text);
    let mut tokens = Vec::new();
    for (value, at) in class_attribute_values(&text) {
        if value.contains("{{") {
            return Err(format!(
                "a class attribute interpolates a value (byte {at}): {value:?}. \
                 The guard cannot know which classes that renders, so it would \
                 be blind to them. Render the class list as a literal, or teach \
                 this guard the source of the interpolated value — do not make \
                 the check pass by ignoring it."
            ));
        }
        let literal = strip_askama_blocks(value);
        tokens.extend(literal.split_whitespace().map(str::to_string));
    }
    Ok(tokens)
}

// -----------------------------------------------------------------------------
// JavaScript: `className = "…"` and `classList.add/remove/toggle(…)`
// -----------------------------------------------------------------------------

/// The string literal starting at `at`, plus the index just past it. A
/// backslash escape never lets the terminator through.
fn string_literal_at(text: &str, at: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    let quote = *bytes.get(at)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let start = at + 1;
    let mut end = start;
    while end < bytes.len() && bytes[end] != quote {
        end += if bytes[end] == b'\\' { 2 } else { 1 };
    }
    Some((&text[start..end.min(bytes.len())], end.min(bytes.len()) + 1))
}

/// Every double- or single-quoted string literal in `text`.
fn string_literals(text: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut at = 0;
    while at < text.len() {
        match string_literal_at(text, at) {
            Some((literal, next)) => {
                literals.push(literal.to_string());
                at = next;
            }
            None => at += 1,
        }
    }
    literals
}

/// The top-level comma-separated arguments of a call whose `(` is at `open`.
/// Commas nested in brackets, braces, parentheses or string literals do not
/// split, so `add('a', fn('b'))` is two arguments and not three.
fn call_arguments(text: &str, open: usize) -> (Vec<&str>, usize) {
    let bytes = text.as_bytes();
    let mut arguments = Vec::new();
    let mut depth = 0usize;
    let mut start = open + 1;
    let mut at = open;
    while at < bytes.len() {
        match bytes[at] {
            b'"' | b'\'' => {
                // Step over the whole literal so a comma inside it is not a
                // separator.
                at = match string_literal_at(text, at) {
                    Some((_, next)) => next - 1,
                    None => at,
                };
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 && bytes[at] == b')' {
                    arguments.push(&text[start..at]);
                    return (arguments, at + 1);
                }
            }
            b',' if depth == 1 => {
                arguments.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    (arguments, text.len())
}

/// Class tokens from a JavaScript scan source.
///
/// Anchored on the two ways this codebase writes classes into the DOM, so
/// prose cannot become a class however class-shaped it is. `static/picker.js`
/// opens with a 37-line block comment naming `htmx-request`, `htmx-indicator`
/// and `state`; only an anchored reader is immune to that.
///
/// An argument counts as a class name only when it *is* a string literal, which
/// is what keeps a non-class argument out: in
/// `busy.classList.toggle('htmx-request', status === 'searching')` the second
/// argument does not start with a quote, so it is skipped without the trailing
/// argument having to be removed. `toggle`'s optional force argument is dropped
/// anyway whenever more than one argument is present, so the intent survives an
/// argument that happens to be written as a literal. The length check matters:
/// `toggle('htmx-request')` has no force argument, and popping unconditionally
/// made that call contribute nothing at all.
fn js_class_tokens(text: &str) -> Vec<String> {
    let text = strip_js_block_comments(text);
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let assignment = rest.find("className");
        let method_call = rest.find("classList.");
        let (at, is_class_list) = match (assignment, method_call) {
            (Some(a), Some(b)) if a <= b => (a, false),
            (Some(_), Some(b)) => (b, true),
            (Some(a), None) => (a, false),
            (None, Some(b)) => (b, true),
            (None, None) => break,
        };

        let anchor = cursor + at;
        if is_class_list {
            // `classList.<writer>(…)`. A reader such as `contains` or `item`
            // matches no writer, so the cursor steps past it and the scan
            // continues rather than mistaking it for a `className` anchor.
            let Some(writer) = class_list_writer(&text[anchor..]) else {
                cursor = anchor + "classList.".len();
                continue;
            };
            let open = anchor + "classList.".len() + writer.len();
            let (mut arguments, next) = call_arguments(&text, open);
            if writer == "toggle" && arguments.len() > 1 {
                // `toggle(names…, force)`: the trailing argument is the force
                // flag. Only when there is a name before it — with one argument
                // there is no flag, and that argument is the class.
                arguments.pop();
            }
            for argument in arguments {
                let argument = argument.trim();
                if let Some((literal, _)) = string_literal_at(argument, 0) {
                    tokens.extend(literal.split_whitespace().map(str::to_string));
                }
            }
            cursor = next;
            continue;
        }

        // `className = "…"`, with whitespace and newlines around the `=`.
        let bytes = text.as_bytes();
        let mut at = anchor + "className".len();
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at < bytes.len() && bytes[at] == b'=' {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            if let Some((literal, next)) = string_literal_at(&text, at) {
                tokens.extend(literal.split_whitespace().map(str::to_string));
                cursor = next;
                continue;
            }
        }
        cursor = anchor + "className".len();
    }
    tokens
}

/// The `classList` methods that write class names. This is the only list of
/// them: `class_list_writer` is the single place a writer is resolved.
const CLASS_LIST_WRITERS: [&str; 4] = ["add", "remove", "toggle", "replace"];

/// The writer whose call starts at `anchor`, or `None` when the `classList.`
/// there is a reader (`contains`, `item`) or a method this guard does not
/// model. The writer must be followed by `(`, so a longer method name that
/// merely starts with a writer — a future `classList.toggleAll` — is not
/// mistaken for `toggle`.
fn class_list_writer(anchor: &str) -> Option<&'static str> {
    let after = anchor.strip_prefix("classList.")?;
    CLASS_LIST_WRITERS.into_iter().find(|writer| {
        after
            .strip_prefix(writer)
            .is_some_and(|rest| rest.starts_with('('))
    })
}

/// Remove `/* … */` comments. Line comments are deliberately left alone: the
/// extraction above is anchored on `className` and `classList`, so a line
/// comment cannot contribute a class anyway, and skipping `//` naively would
/// mangle a URL or a regular expression literal.
fn strip_js_block_comments(text: &str) -> String {
    strip_css_comments(text)
}

// -----------------------------------------------------------------------------
// The compiled stylesheet: every class selector, unescaped
// -----------------------------------------------------------------------------

/// At-rules whose block holds style rules rather than declarations.
const NESTING_AT_RULES: [&str; 7] = [
    "@media",
    "@supports",
    "@layer",
    "@container",
    "@scope",
    "@document",
    "@starting-style",
];

fn is_css_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// A byte that ends a class selector: whitespace, a combinator, the punctuation
/// that delimits a compound selector, or the start of a pseudo-class. An
/// *escaped* one of these characters is part of the name instead, which is the
/// whole reason the scan is escape-aware.
fn ends_class_name(byte: u8) -> bool {
    is_css_whitespace(byte)
        || matches!(
            byte,
            b'>' | b'+'
                | b'~'
                | b','
                | b'.'
                | b'*'
                | b'('
                | b')'
                | b'['
                | b']'
                | b'{'
                | b'}'
                | b';'
                | b':'
                | b'='
                | b'^'
                | b'$'
                | b'|'
                | b'"'
                | b'!'
                | b'#'
                | b'%'
                | b'&'
                | b'\''
                | b'<'
                | b'?'
                | b'/'
                | b'\\'
                | b'@'
        )
}

/// The index just past an escape sequence starting at `at` (which is the
/// backslash): up to six hex digits with one optional trailing whitespace, or
/// the single escaped character.
fn past_escape(bytes: &[u8], at: usize) -> usize {
    let mut cursor = at + 1;
    let hex = bytes[cursor..]
        .iter()
        .take(6)
        .take_while(|b| b.is_ascii_hexdigit())
        .count();
    if hex > 0 {
        cursor += hex;
        if cursor < bytes.len() && is_css_whitespace(bytes[cursor]) {
            cursor += 1;
        }
    } else if cursor < bytes.len() {
        cursor += 1;
    }
    cursor
}

/// The raw (still escaped) class selector starting at `start`, which is the byte
/// after a `.`, plus the index just past it.
fn raw_class_name(css: &str, start: usize) -> (&str, usize) {
    let bytes = css.as_bytes();
    let mut at = start;
    while at < bytes.len() {
        if bytes[at] == b'\\' {
            at = past_escape(bytes, at);
            continue;
        }
        if ends_class_name(bytes[at]) {
            break;
        }
        at += 1;
    }
    (&css[start..at], at)
}

/// Resolve CSS escape sequences: `\:` is `:`, `\[` is `[`, `\.` is `.`, and
/// `\[41\]` is `A`. This is the inverse of what the compiler did to
/// `sm:grid-cols-[100px_1fr_120px_auto]`, and it is why the lookup below is a
/// plain set membership test.
fn css_unescape(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\\' {
            let end = past_escape(bytes, at);
            let escaped = &raw[at + 1..end.min(raw.len())];
            match u32::from_str_radix(
                escaped.trim_end_matches(|c: char| is_css_whitespace(c as u8)),
                16,
            ) {
                Ok(code) => {
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                // A non-hex escape is the escaped character itself.
                Err(_) => out.push_str(escaped),
            }
            at = end;
        } else {
            let ch = raw[at..].chars().next().expect("in-bounds char");
            out.push(ch);
            at += ch.len_utf8();
        }
    }
    out
}

/// The class names a selector list defines.
fn class_names_in_selector(selector: &str) -> Vec<String> {
    let bytes = selector.as_bytes();
    let mut names = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'.' {
            let (raw, next) = raw_class_name(selector, at + 1);
            if !raw.is_empty() {
                names.push(css_unescape(raw));
            }
            at = next;
        } else {
            at += 1;
        }
    }
    names
}

/// The index of the first `needle` at or after `at`, skipping quoted strings so
/// a brace or bracket inside a declaration value cannot be mistaken for
/// structure.
fn find_outside_strings(bytes: &[u8], at: usize, needle: u8) -> Option<usize> {
    let mut cursor = at;
    while cursor < bytes.len() {
        match bytes[cursor] {
            quote @ (b'"' | b'\'') => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor] != quote {
                    cursor += 1;
                }
            }
            byte if byte == needle => return Some(cursor),
            _ => {}
        }
        cursor += 1;
    }
    None
}

/// The index of the `}` matching the `{` at `open`.
fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut cursor = open;
    while cursor < bytes.len() {
        match bytes[cursor] {
            quote @ (b'"' | b'\'') => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor] != quote {
                    cursor += 1;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(cursor);
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn collect_class_names(css: &str, out: &mut BTreeSet<String>, depth: usize) {
    assert!(depth < 64, "compiled stylesheet nests too deeply to parse");
    let bytes = css.as_bytes();
    let mut at = 0;
    while let Some(open) = find_outside_strings(bytes, at, b'{') {
        let prelude = css[at..open].trim();
        let close = matching_brace(bytes, open).unwrap_or_else(|| {
            panic!("unbalanced braces in the compiled stylesheet at byte {open}")
        });
        if let Some(rule) = prelude.strip_prefix('@') {
            // A nesting at-rule holds style rules; every other at-rule
            // (`@property`, `@font-face`, `@keyframes`) holds declarations or
            // further at-rules and defines no class selector of its own.
            if NESTING_AT_RULES
                .iter()
                .any(|nesting| rule.starts_with(&nesting[1..]))
            {
                collect_class_names(&css[open + 1..close], out, depth + 1);
            }
        } else if !prelude.is_empty() {
            out.extend(class_names_in_selector(prelude));
        }
        at = close + 1;
    }
}

/// Every class name the compiled stylesheet defines, unescaped.
fn compiled_class_names(css: &str) -> BTreeSet<String> {
    let css = strip_css_comments(css);
    let mut names = BTreeSet::new();
    collect_class_names(&css, &mut names, 0);
    names
}

/// The class tokens every `@source` path contributes, mapped to the scanned
/// files that use them so a failure names the markup, not just the class.
fn used_class_tokens() -> BTreeMap<String, Vec<String>> {
    let root = repo_root();
    let mut used: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for source in declared_scan_sources(&root.join(ENTRYPOINT)) {
        for file in source_files(&source) {
            let shown = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string();
            for token in class_tokens(&file) {
                used.entry(token).or_default().push(shown.clone());
            }
        }
    }
    used
}

// -----------------------------------------------------------------------------
// The guard
// -----------------------------------------------------------------------------

/// U1: every utility class a scanned source uses has a rule in the committed
/// stylesheet. This is the assertion the stale stylesheet failed.
#[test]
fn every_class_used_by_a_scanned_source_has_a_rule_in_the_committed_stylesheet() {
    let root = repo_root();
    let compiled = root.join(COMPILED);
    let defined = compiled_class_names(&read(&compiled));
    let used = used_class_tokens();

    let missing: Vec<String> = used
        .iter()
        .filter(|(token, _)| !defined.contains(*token))
        .map(|(token, sites)| {
            let mut sites: Vec<&str> = sites.iter().map(String::as_str).collect();
            sites.sort_unstable();
            sites.dedup();
            format!("  {token}  (used in {})", sites.join(", "))
        })
        .collect();

    assert!(
        missing.is_empty(),
        "{} of the {} class tokens used by a scanned source have no rule in \
         the committed {COMPILED}, so those elements render unstyled today:\n{}\n\
         The input is correct; regenerate the output with scripts/build-css.sh.",
        missing.len(),
        used.len(),
        missing.join("\n")
    );

    println!(
        "stylesheet guard: {} class names defined, {} tokens checked across {} \
         scanned source path(s), 0 missing",
        defined.len(),
        used.len(),
        declared_scan_sources(&root.join(ENTRYPOINT)).len()
    );
}

/// The guard is only worth having if it actually reads something. A `templates/`
/// that failed to resolve, an entrypoint whose `@source` lines stopped being
/// parsed, or a file type with no extractor would each turn the guard above
/// into a vacuous pass, so the shape of the scan is pinned here.
#[test]
fn the_stylesheet_scan_covers_both_declared_sources_and_is_not_vacuous() {
    let root = repo_root();
    let sources = declared_scan_sources(&root.join(ENTRYPOINT));
    let shown: Vec<String> = sources
        .iter()
        .map(|p| p.strip_prefix(&root).unwrap_or(p).display().to_string())
        .collect();

    assert_eq!(
        shown,
        vec!["templates".to_string(), "static/picker.js".to_string()],
        "the guard reads the entrypoint's @source declarations, so this is the \
         scan surface it actually covers. Update this test deliberately when a \
         source is added or removed."
    );

    let mut files = 0usize;
    for source in &sources {
        files += source_files(source).len();
    }
    assert!(
        files >= 50,
        "the walk found only {files} scanned files; the template tree alone is \
         larger than that, so the walk is not seeing the tree"
    );

    let defined = compiled_class_names(&read(&root.join(COMPILED)));
    assert!(
        defined.len() >= 200,
        "only {} class names parsed out of {COMPILED}; the compiled stylesheet \
         defines far more, so the selector parser is not reading it",
        defined.len()
    );

    let used = used_class_tokens();
    assert!(
        used.len() >= 200,
        "only {} class tokens collected; the template tree uses far more, so \
         the extraction is not reading it",
        used.len()
    );

    // One known-present token per source, so an extractor that silently returns
    // nothing cannot pass on the count alone.
    for token in ["btn-secondary", "justify-center", "sr-only", "htmx-request"] {
        assert!(
            used.contains_key(token),
            "`{token}` is used by a scanned source today but the guard did not \
             collect it; the extraction is broken, not merely incomplete"
        );
    }
    for token in ["btn-secondary", "field", "chip", "sr-only"] {
        assert!(
            defined.contains(token),
            "`{token}` is compiled today but the guard did not read its rule; \
             the selector parser is broken, not merely incomplete"
        );
    }
}

/// An Askama control block is syntax, not a class list, and ten class
/// attributes in this tree carry a raw `"` inside one. Both facts have to hold
/// or the guard reports `if`, `endif` and half an expression as missing classes.
#[test]
fn class_attribute_extraction_drops_askama_control_blocks() {
    let tokens = html_class_tokens(
        r#"<span class="chip {% if product.kind.to_string() == "Service" %}chip-expense{% else %}chip-income{% endif %}">x</span>"#,
    )
    .expect("no interpolation");
    assert_eq!(tokens, ["chip", "chip-expense", "chip-income"]);
}

/// Prose is not a class list. A template that documents a class in a comment is
/// the ordinary case, and Tailwind's own scanner would compile it — the guard
/// must not, or every documented class becomes a false positive.
#[test]
fn class_attribute_extraction_ignores_comments() {
    let tokens = html_class_tokens(
        r#"{# the old layout used grid-cols-3 here #}
           <!-- <div class="grid-cols-3 stale-utility"></div> -->
           <div class="grid-cols-2"></div>"#,
    )
    .expect("no interpolation");
    assert_eq!(tokens, ["grid-cols-2"]);
}

/// An interpolated class value renders to something the guard cannot predict.
/// Skipping it silently would trade a loud blind spot for a quiet one.
#[test]
fn class_attribute_extraction_refuses_an_interpolated_class_value() {
    let err = html_class_tokens(r#"<div class="flex {{ extra }}"></div>"#)
        .expect_err("an interpolated class value must be refused, not skipped");
    assert!(
        err.contains("{{ extra }}"),
        "the failure must show the offending attribute: {err}"
    );
}

/// The escape-aware half of the guard. Every one of these compiles to an
/// escaped selector, and a substring search finds none of them.
#[test]
fn compiled_class_names_unescape_arbitrary_values_and_variants() {
    let defined = compiled_class_names(
        r#"/*! tailwindcss v4.3.3 | MIT License | https://tailwindcss.com */
           .py-2\.5{padding-block:calc(var(--spacing) * 2.5)}
           .sm\:grid-cols-2{grid-template-columns:repeat(2,minmax(0,1fr))}
           @media (min-width:40rem){.sm\:grid-cols-\[100px_1fr_120px_auto\]{grid-template-columns:100px 1fr 120px auto}}
           .min-h-\[70vh\]{min-height:70vh}
           .border-danger\/30{border-color:#f871714d}
           @supports (color:color-mix(in lab, red, red)){.\[\&\>\*\]\:flex-1>*{flex:1}}
           @media (hover:hover){.hover\:bg-card:hover{background-color:var(--color-card)}}
           .container{width:100%}"#,
    );
    for token in [
        "py-2.5",
        "sm:grid-cols-2",
        "sm:grid-cols-[100px_1fr_120px_auto]",
        "min-h-[70vh]",
        "border-danger/30",
        "[&>*]:flex-1",
        "hover:bg-card",
        "container",
    ] {
        assert!(defined.contains(token), "`{token}` must be read out of CSS");
    }
    // The licence banner is not a selector, and without comment stripping its
    // `v4.3.3` and `tailwindcss.com` each yield one.
    for junk in ["3.3", "com", "tailwindcss", "MIT License"] {
        assert!(
            !defined.contains(junk),
            "`{junk}` is comment text, not a class selector"
        );
    }
}

/// The island renders its classes from JavaScript, which is why
/// `assets/tailwind.css` declares a second `@source` for it. Both ways this
/// codebase writes classes must be read, and the argument list of a
/// `classList` call is not entirely class names.
#[test]
fn js_class_extraction_reads_assignments_and_class_list_calls() {
    let tokens = js_class_tokens(
        r#"
        /*
         * The busy cue shows through the htmx-indicator pair: the island adds
         * `htmx-request` while in flight, exactly what htmx used to toggle.
         * State lives in `state.status`, not in a class like state-query.
         */
        btn.className =
          'btn-secondary flex w-full select-none items-center';
        p.className = 'sr-only';
        busy.classList.toggle('htmx-request', state.status === 'searching');
        list.classList.add('flex', 'flex-col');
        sidebar.classList.contains('-translate-x-full');
        var url = 'https://example.test/a//b';
        "#,
    );
    let unique: BTreeSet<&str> = tokens.iter().map(String::as_str).collect();
    for token in [
        "btn-secondary",
        "flex",
        "w-full",
        "select-none",
        "items-center",
        "sr-only",
        "htmx-request",
        "flex-col",
    ] {
        assert!(
            unique.contains(token),
            "`{token}` must be collected; got {tokens:?}"
        );
    }
    for junk in [
        "htmx-indicator",
        "state-query",
        "-translate-x-full",
        "https://example.test/a//b",
        "contains",
    ] {
        assert!(
            !unique.contains(junk),
            "`{junk}` is prose or a non-class argument, not a class token"
        );
    }
}

/// F1: a one-argument `classList.toggle(name)` is the whole common case, and it
/// must contribute its class. An earlier version popped the trailing argument
/// unconditionally to skip `toggle`'s optional force flag, so a single-argument
/// toggle contributed nothing at all — a silent false negative, the worst kind,
/// because the guard stayed green while checking one class fewer.
#[test]
fn a_single_argument_class_list_toggle_still_contributes_its_class() {
    let one = js_class_tokens("busy.classList.toggle('htmx-request');");
    assert_eq!(
        one,
        ["htmx-request"],
        "toggle with only a class name must yield that class, not nothing"
    );

    // The force form is the one that needs the trailing argument dropped.
    let two = js_class_tokens("busy.classList.toggle('htmx-request', status === 'searching');");
    assert_eq!(
        two,
        ["htmx-request"],
        "the boolean force argument is not a class and must not be reported"
    );

    // One token with a force flag, and several class names at once.
    assert_eq!(
        js_class_tokens("el.classList.toggle('flex', force);"),
        ["flex"],
        "a single class plus the force flag still yields the class"
    );
    assert_eq!(
        js_class_tokens("el.classList.add('flex', 'gap-2', 'sr-only');"),
        ["flex", "gap-2", "sr-only"],
        "every class name in a multi-argument add must be collected"
    );
    assert_eq!(
        js_class_tokens("el.classList.remove('flex');"),
        ["flex"],
        "remove writes too: it takes the class off, so the class was applied"
    );
    assert_eq!(
        js_class_tokens("el.classList.replace('flex', 'grid');"),
        ["flex", "grid"],
        "replace writes both names"
    );
}

/// The `(` requirement in `class_list_writer`, pinned.
///
/// A writer name is a prefix of longer method names: `toggleAll` starts with
/// `toggle`, `replaceAll` with `replace`. Resolving on `starts_with` alone read
/// both as the shorter writer, and the mis-parse is not always silent — the
/// argument span then starts mid-identifier, which drops the classes a real
/// call would have contributed, and can run past the end of the file when the
/// trailing parentheses do not balance. So a writer must be followed by `(`.
#[test]
fn a_class_list_method_merely_starting_with_a_writer_is_not_a_writer() {
    // The resolver itself: `toggleAll` is not `toggle`, and a reader is not a
    // writer. This is the line the fix changed.
    assert_eq!(
        class_list_writer("classList.toggleAll('flex')"),
        None,
        "toggleAll merely starts with toggle; it is not a writer this guard models"
    );
    assert_eq!(
        class_list_writer("classList.toggle('flex')"),
        Some("toggle"),
        "toggle followed by `(` is the writer"
    );
    for reader in ["classList.contains('x')", "classList.item(0)"] {
        assert_eq!(
            class_list_writer(reader),
            None,
            "`{reader}` reads a class, it does not write one"
        );
    }

    // A non-writer contributes nothing, and an unterminated one must not crash
    // the scan on the way to contributing nothing.
    for source in [
        "el.classList.toggleAll('flex');",
        "el.classList.toggleAll(",
        "el.classList.toggleAll();",
        "el.classList.contains('x');",
        "el.classList.item(0);",
    ] {
        assert_eq!(
            js_class_tokens(source),
            Vec::<String>::new(),
            "`{source}` is not a writer this guard models, so it yields no class"
        );
    }

    // The real writer is unaffected by the check beside it.
    assert_eq!(
        js_class_tokens("el.classList.toggle('flex');"),
        ["flex"],
        "toggle is still read as a writer"
    );

    // These two are what make the test bite. Read as `replace`, the argument
    // span of `replaceAll('a', 'b')` starts mid-identifier, so the first
    // argument is dropped as unparseable and the second — `b` — is reported as
    // a class the source never applies.
    assert_eq!(
        js_class_tokens("el.classList.replaceAll('a', 'b');"),
        Vec::<String>::new(),
        "replaceAll merely starts with replace; reporting `b` would be a false \
         positive for a class nothing applies"
    );

    // And the false negative: read as `toggle`, the span of an unterminated
    // `toggleAll('a' …` never closes, so the scan consumed the rest of the file
    // and lost every class after it.
    assert_eq!(
        js_class_tokens("el.classList.toggleAll('a' el.classList.toggle('b');"),
        ["b"],
        "a non-writer must not swallow the calls that follow it"
    );
}

/// `assets/tailwind.css` states that base.html's inline notice builder is
/// covered because its boxes use the `.notice` component classes, which live in
/// the entrypoint's `@layer components` and are therefore always compiled. The
/// guard reads class *attributes*, so an inline script is outside its net; this
/// is the one place that claim is checked instead of trusted.
#[test]
fn the_base_notice_builder_uses_component_classes_the_stylesheet_always_compiles() {
    let root = repo_root();
    let base = read(&root.join("templates/base.html"));
    let builder = base
        .split("box.className")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_else(|| {
            panic!(
                "templates/base.html no longer builds its notice box with \
                 `box.className`; if it moved, check the claim this test pins"
            )
        });
    let tokens = string_literals(builder);
    assert!(
        tokens.contains(&"notice notice-error".to_string())
            && tokens.contains(&"notice notice-success".to_string()),
        "the notice builder's class lists changed: {tokens:?}"
    );

    let defined = compiled_class_names(&read(&root.join(COMPILED)));
    for component in ["notice", "notice-error", "notice-success"] {
        assert!(
            defined.contains(component),
            "`.{component}` is a component class in the entrypoint's @layer \
             components and must be in the compiled stylesheet regardless of \
             any scan source"
        );
    }
}
