//! Symfony-style dotenv: a Rust port of `symfony/dotenv` 8.1 parsing plus the
//! chain loader (`<base>` → `<base>.local` → `<base>.<stage>` → `<base>.<stage>.local`),
//! resolved maps, per-container filters, and the reserved-key guard (spec §5.2).
//! The ported Symfony test suite is the conformance oracle (spec TC-031);
//! `$(command)` is lexed for Symfony-exact errors but never executed.
//!
//! 8.1 port notes: values are lexed *raw* with literal `$` protected as a NUL
//! marker (`\x00`), then resolved — eagerly per value in [`parse_document`]
//! (Symfony `parse()`), or deferred over the whole chain in [`resolve_documents`]
//! (Symfony `load()`/`loadEnv()` + `resolveLoadedVars()`): up to 5 passes to a
//! fixpoint, so a later layer can override a variable referenced by an earlier
//! one and forward references across layers resolve; values still changing after
//! 5 passes are a circular reference and error. NUL bytes in input are rejected
//! (they would collide with the marker), exactly as Symfony rejects them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use regex_lite::Regex;

use crate::error::{DcdError, Result};

/// Symfony `Dotenv::parse()`: lex + eager per-value resolution. Process env
/// wins over same-document definitions during expansion, like `$_ENV` does.
pub fn parse_document(
    data: &str,
    path: &str,
    process_env: &HashMap<String, String>,
) -> Result<IndexMap<String, String>> {
    Parser::new(data, path).parse_eager(process_env)
}

struct Parser {
    data: Vec<u8>,
    path: String,
    cursor: usize,
    lineno: usize,
    end: usize,
}

impl Parser {
    fn new(data: &str, path: &str) -> Self {
        let normalized = data.replace("\r\n", "\n").replace('\r', "\n");
        let bytes = normalized.into_bytes();
        let end = bytes.len();
        Parser {
            data: bytes,
            path: path.to_string(),
            cursor: 0,
            lineno: 1,
            end,
        }
    }

    fn check_prologue(&self) -> Result<()> {
        if self.data.starts_with(&[0xEF, 0xBB, 0xBF]) {
            return Err(self.format_error_at(
                "Loading files starting with a byte-order-mark (BOM) is not supported.",
                1,
                0,
            ));
        }
        if self.data.contains(&0) {
            return Err(self.format_error_at(
                "Loading files containing NUL bytes is not supported.",
                1,
                0,
            ));
        }
        Ok(())
    }

    /// Symfony `parse()`: each value is resolved right after it is lexed, so
    /// resolution errors carry the cursor position past the value — byte-exact
    /// with the PHP error contexts.
    fn parse_eager(&mut self, process_env: &HashMap<String, String>) -> Result<IndexMap<String, String>> {
        self.check_prologue()?;
        self.skip_empty_lines();

        let mut values: IndexMap<String, String> = IndexMap::new();
        let mut pending_name: Option<String> = None;
        while self.cursor < self.end {
            match pending_name.take() {
                None => pending_name = Some(self.lex_varname()?),
                Some(name) => {
                    let raw = self.lex_value()?;
                    let site = self.error_site();
                    let resolved = {
                        let mut lookup = EagerLookup {
                            process_env,
                            values: &mut values,
                        };
                        resolve_raw_value(&raw, &mut lookup, &|message| site.error(message))?
                    };
                    values.insert(name, resolved);
                }
            }
        }
        if let Some(name) = pending_name {
            values.insert(name, String::new());
        }

        Ok(values)
    }

    /// Symfony `parseRaw()`: lexing only; values keep their `\x00` markers and
    /// escaped backslashes for the deferred chain resolution.
    fn parse_raw(&mut self) -> Result<IndexMap<String, String>> {
        self.check_prologue()?;
        self.skip_empty_lines();

        let mut values: IndexMap<String, String> = IndexMap::new();
        let mut pending_name: Option<String> = None;
        while self.cursor < self.end {
            match pending_name.take() {
                None => pending_name = Some(self.lex_varname()?),
                Some(name) => {
                    let raw = self.lex_value()?;
                    values.insert(name, raw);
                }
            }
        }
        if let Some(name) = pending_name {
            values.insert(name, String::new());
        }

        Ok(values)
    }

    fn byte(&self, index: usize) -> u8 {
        if index < self.end {
            self.data[index]
        } else {
            0
        }
    }

    fn format_error(&self, message: &str) -> DcdError {
        self.format_error_at(message, self.lineno, self.cursor)
    }

    /// Byte-identical to Symfony's `FormatException` + `FormatExceptionContext`.
    fn format_error_at(&self, message: &str, lineno: usize, cursor: usize) -> DcdError {
        DcdError::Config(format!(
            "{message}{}",
            render_error_context(&self.data, self.end, &self.path, lineno, cursor)
        ))
    }

    /// Freezes the current error context so resolution can report errors after
    /// the lexer state is borrowed elsewhere.
    fn error_site(&self) -> ErrorSite {
        ErrorSite {
            rendered_context: render_error_context(
                &self.data,
                self.end,
                &self.path,
                self.lineno,
                self.cursor,
            ),
        }
    }

    /// Mirrors Symfony's `skipEmptyLines` (PHP `\s`, which includes vertical
    /// tab and form feed) — the only place line numbers advance, bug-compatible
    /// with the PHP lexer's raw cursor arithmetic elsewhere.
    fn skip_empty_lines(&mut self) {
        loop {
            let start = self.cursor;
            while self.cursor < self.end
                && matches!(self.byte(self.cursor), b' ' | b'\t' | b'\n' | 0x0B | 0x0C)
            {
                if self.byte(self.cursor) == b'\n' {
                    self.lineno += 1;
                }
                self.cursor += 1;
            }
            if self.cursor < self.end && self.byte(self.cursor) == b'#' {
                while self.cursor < self.end && self.byte(self.cursor) != b'\n' {
                    self.cursor += 1;
                }
            }
            if self.cursor == start {
                return;
            }
        }
    }

    /// Symfony 8.1 `VARNAME_REGEX`: `(?i:_*[A-Z][A-Z0-9_]*+)`.
    fn match_varname(&self, at: usize) -> Option<(String, usize)> {
        let mut index = at;
        while index < self.end && self.byte(index) == b'_' {
            index += 1;
        }
        if !self.byte(index).is_ascii_alphabetic() {
            return None;
        }
        index += 1;
        while index < self.end
            && (self.data[index].is_ascii_alphanumeric() || self.data[index] == b'_')
        {
            index += 1;
        }
        let name = String::from_utf8_lossy(&self.data[at..index]).into_owned();
        Some((name, index))
    }

    fn lex_varname(&mut self) -> Result<String> {
        let mut exported = false;
        let mut name_and_end = None;

        if self.data[self.cursor..].starts_with(b"export") {
            let mut index = self.cursor + 6;
            let mut spaces = 0;
            while matches!(self.byte(index), b' ' | b'\t') {
                index += 1;
                spaces += 1;
            }
            if spaces > 0 {
                if let Some(found) = self.match_varname(index) {
                    exported = true;
                    name_and_end = Some(found);
                }
            }
        }
        if name_and_end.is_none() {
            name_and_end = self.match_varname(self.cursor);
        }
        let Some((name, after_name)) = name_and_end else {
            return Err(self.format_error("Invalid character in variable name"));
        };
        self.cursor = after_name;

        if self.cursor == self.end || matches!(self.byte(self.cursor), b'\n' | b'#') {
            if exported {
                return Err(self.format_error("Unable to unset an environment variable"));
            }
            return Err(self.format_error("Missing = in the environment variable declaration"));
        }
        if matches!(self.byte(self.cursor), b' ' | b'\t') {
            return Err(
                self.format_error("Whitespace characters are not supported after the variable name")
            );
        }
        if self.byte(self.cursor) != b'=' {
            return Err(self.format_error("Missing = in the environment variable declaration"));
        }
        self.cursor += 1;
        Ok(name)
    }

    /// Symfony 8.1 `lexValue`: raw segments. Single quotes double backslashes
    /// and mark `$` literal; double quotes unescape then protect `\$`; unquoted
    /// segments protect `\$` and only refuse spaces when no `$` is involved.
    fn lex_value(&mut self) -> Result<String> {
        let mut probe = self.cursor;
        while matches!(self.byte(probe), b' ' | b'\t') {
            probe += 1;
        }
        if probe == self.end || self.byte(probe) == b'\n' {
            self.cursor = probe;
            self.skip_empty_lines();
            return Ok(String::new());
        }
        if self.byte(probe) == b'#' {
            while probe < self.end && self.byte(probe) != b'\n' {
                probe += 1;
            }
            self.cursor = probe;
            self.skip_empty_lines();
            return Ok(String::new());
        }
        if matches!(self.byte(self.cursor), b' ' | b'\t') {
            return Err(self.format_error("Whitespace are not supported before the value"));
        }

        let mut assembled = String::new();
        loop {
            match self.byte(self.cursor) {
                b'\'' => {
                    let mut len = 0;
                    loop {
                        len += 1;
                        if self.cursor + len == self.end {
                            self.cursor += len;
                            return Err(self.format_error("Missing quote to end the value"));
                        }
                        if self.byte(self.cursor + len) == b'\'' {
                            break;
                        }
                    }
                    let raw = String::from_utf8_lossy(&self.data[self.cursor + 1..self.cursor + len])
                        .into_owned();
                    assembled.push_str(&raw.replace('\\', "\\\\").replace('$', "\0"));
                    self.cursor += 1 + len;
                }
                b'"' => {
                    let mut raw = Vec::new();
                    self.cursor += 1;
                    if self.cursor == self.end {
                        return Err(self.format_error("Missing quote to end the value"));
                    }
                    loop {
                        let current = self.byte(self.cursor);
                        let prev = if self.cursor >= 1 { self.byte(self.cursor - 1) } else { 0 };
                        let prev2 = if self.cursor >= 2 { self.byte(self.cursor - 2) } else { 0 };
                        if current == b'"' && !(prev == b'\\' && prev2 != b'\\') {
                            break;
                        }
                        raw.push(current);
                        self.cursor += 1;
                        if self.cursor == self.end {
                            return Err(self.format_error("Missing quote to end the value"));
                        }
                    }
                    self.cursor += 1;
                    let unescaped = String::from_utf8_lossy(&raw)
                        .replace("\\\"", "\"")
                        .replace("\\r", "\r")
                        .replace("\\n", "\n");
                    assembled.push_str(&protect_escaped_dollars(&unescaped));
                }
                _ => {
                    let mut raw = Vec::new();
                    let mut prev = if self.cursor >= 1 { self.byte(self.cursor - 1) } else { 0 };
                    while self.cursor < self.end
                        && !matches!(self.byte(self.cursor), b'\n' | b'"' | b'\'')
                        && !(matches!(prev, b' ' | b'\t') && self.byte(self.cursor) == b'#')
                    {
                        if self.byte(self.cursor) == b'\\'
                            && matches!(self.byte(self.cursor + 1), b'"' | b'\'')
                        {
                            self.cursor += 1;
                        }
                        raw.push(self.byte(self.cursor));
                        prev = self.byte(self.cursor);
                        if self.byte(self.cursor) == b'$' && self.byte(self.cursor + 1) == b'(' {
                            self.cursor += 1;
                            let nested = self.lex_nested_expression(0)?;
                            raw.push(b'(');
                            raw.extend_from_slice(&nested);
                            raw.push(b')');
                        }
                        self.cursor += 1;
                    }
                    let trimmed = {
                        let text = String::from_utf8_lossy(&raw).into_owned();
                        text.trim_end_matches([' ', '\t', '\n', '\r', '\0', '\u{B}']).to_string()
                    };
                    let protected = protect_escaped_dollars(&trimmed);
                    if protected == trimmed
                        && contains_ascii_whitespace(&trimmed)
                        && !trimmed.contains('$')
                    {
                        return Err(
                            self.format_error("A value containing spaces must be surrounded by quotes")
                        );
                    }
                    assembled.push_str(&protected);
                    if self.cursor < self.end && self.byte(self.cursor) == b'#' {
                        break;
                    }
                }
            }
            if !(self.cursor < self.end && self.byte(self.cursor) != b'\n') {
                break;
            }
        }
        self.skip_empty_lines();
        Ok(assembled)
    }

    fn lex_nested_expression(&mut self, depth: usize) -> Result<Vec<u8>> {
        if depth > self.max_nesting_depth() {
            return Err(self.format_error("Missing closing parenthesis."));
        }
        self.cursor += 1;
        if self.cursor >= self.end {
            return Err(self.format_error("Missing closing parenthesis."));
        }
        let mut collected: Vec<u8> = Vec::new();
        while self.byte(self.cursor) != b'\n' && self.byte(self.cursor) != b')' {
            collected.push(self.data[self.cursor]);
            if self.data[self.cursor] == b'(' {
                let nested = self.lex_nested_expression(depth + 1)?;
                collected.extend_from_slice(&nested);
                collected.push(b')');
            }
            self.cursor += 1;
            if self.cursor >= self.end {
                return Err(self.format_error("Missing closing parenthesis."));
            }
        }
        if self.byte(self.cursor) == b'\n' {
            return Err(self.format_error("Missing closing parenthesis."));
        }
        Ok(collected)
    }

    /// Caps `$( ( ( …` recursion well below stack exhaustion (~60k frames on an
    /// 8 MB stack); adversarial depth becomes the same Symfony-format error.
    fn max_nesting_depth(&self) -> usize {
        1_000
    }
}

fn render_error_context(data: &[u8], end: usize, path: &str, lineno: usize, cursor: usize) -> String {
    let before_start = cursor.saturating_sub(20);
    let before_window = &data[before_start..cursor.min(end)];
    let before = escape_newlines(before_window);
    let after = escape_newlines(&data[cursor.min(end)..(cursor + 20).min(end)]);
    // PHP pads by strlen of the escaped BYTE window; a lossy replacement char
    // would inflate the char-based length, so count bytes like PHP does.
    let escaped_newlines = before_window.iter().filter(|b| **b == b'\n').count();
    let padding = " ".repeat(before_window.len() + escaped_newlines + 2);
    format!(
        " in \"{path}\" at line {lineno}.\n...{before}{after}...\n{padding}^ line {lineno} offset {cursor}"
    )
}

struct ErrorSite {
    rendered_context: String,
}

impl ErrorSite {
    fn error(&self, message: &str) -> DcdError {
        DcdError::Config(format!("{message}{}", self.rendered_context))
    }
}

/// Symfony 8.1 `protectEscapedDollars`: an odd run of backslashes before `$`
/// escapes it — drop one backslash and mark the `$` literal (`\x00`); an even
/// run leaves the reference live.
fn protect_escaped_dollars(value: &str) -> String {
    if !value.contains('$') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            let ch = value[index..].chars().next().unwrap();
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }
        let run_start = index;
        while index < bytes.len() && bytes[index] == b'\\' {
            index += 1;
        }
        let run = index - run_start;
        if index < bytes.len() && bytes[index] == b'$' {
            if run % 2 == 1 {
                out.push_str(&value[run_start..run_start + run - 1]);
                out.push('\0');
            } else {
                out.push_str(&value[run_start..=index]);
            }
            index += 1;
        } else {
            out.push_str(&value[run_start..index]);
        }
    }
    out
}

/// PHP `\s` without `/u`, as used by Symfony's unquoted-space check.
fn contains_ascii_whitespace(value: &str) -> bool {
    value
        .bytes()
        .any(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C))
}

/// What a variable reference resolves against: the eager per-document mode
/// mirrors Symfony `parse()`, the chain mode mirrors `resolveLoadedVars()`.
trait EnvLookup {
    fn look_up(&self, name: &str) -> VariableLookup;
    fn assign(&mut self, name: &str, value: &str);
}

struct VariableLookup {
    value: String,
    /// Values from outside the parsed document(s) are unescaped and need
    /// protection before substitution; loaded raw values are already protected.
    external: bool,
    loaded: bool,
}

struct EagerLookup<'a> {
    process_env: &'a HashMap<String, String>,
    values: &'a mut IndexMap<String, String>,
}

impl EnvLookup for EagerLookup<'_> {
    fn look_up(&self, name: &str) -> VariableLookup {
        if let Some(value) = self.process_env.get(name) {
            return VariableLookup { value: value.clone(), external: true, loaded: false };
        }
        if let Some(value) = self.values.get(name) {
            return VariableLookup { value: value.clone(), external: false, loaded: false };
        }
        VariableLookup { value: String::new(), external: true, loaded: false }
    }

    fn assign(&mut self, name: &str, value: &str) {
        self.values.insert(name.to_string(), value.to_string());
    }
}

struct ChainLookup<'a> {
    working: &'a IndexMap<String, String>,
    loaded: &'a HashSet<String>,
    assigned: &'a mut IndexMap<String, String>,
}

impl EnvLookup for ChainLookup<'_> {
    fn look_up(&self, name: &str) -> VariableLookup {
        let is_loaded = self.loaded.contains(name);
        if is_loaded {
            if let Some(value) = self.assigned.get(name) {
                return VariableLookup { value: value.clone(), external: false, loaded: true };
            }
        }
        if let Some(value) = self.working.get(name) {
            return VariableLookup { value: value.clone(), external: true, loaded: is_loaded };
        }
        if let Some(value) = self.assigned.get(name) {
            return VariableLookup { value: value.clone(), external: false, loaded: false };
        }
        VariableLookup { value: String::new(), external: true, loaded: false }
    }

    fn assign(&mut self, name: &str, value: &str) {
        self.assigned.insert(name.to_string(), value.to_string());
    }
}

/// Symfony 8.1 `resolveValue`: commands → variables → unescape backslashes →
/// restore literal-`$` markers.
fn resolve_raw_value(
    raw: &str,
    lookup: &mut dyn EnvLookup,
    mkerr: &dyn Fn(&str) -> DcdError,
) -> Result<String> {
    let resolved = resolve_commands_text(raw, mkerr)?;
    let resolved = resolve_variables_text(&resolved, lookup, mkerr)?;
    let resolved = resolved.replace("\\\\", "\\");
    Ok(resolved.replace('\0', "$"))
}

/// Symfony resolves `$(command)` by shell execution; dcd refuses (spec §5.2.1).
/// An escaped `\$(...)` loses the backslash and stays literal, exactly like Symfony.
fn resolve_commands_text(value: &str, mkerr: &dyn Fn(&str) -> DcdError) -> Result<String> {
    if !value.contains('$') {
        return Ok(value.to_string());
    }
    let bytes = value.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$'
            && index + 1 < bytes.len()
            && bytes[index + 1] == b'('
            && balanced_paren_end(bytes, index + 1).is_some()
        {
            let close = balanced_paren_end(bytes, index + 1).unwrap();
            if out.ends_with('\\') {
                out.pop();
                out.push_str(&value[index..=close]);
                index = close + 1;
                continue;
            }
            return Err(mkerr("command expansion is not supported; remove $(...)"));
        }
        let ch = value[index..].chars().next().unwrap();
        out.push(ch);
        index += ch.len_utf8();
    }
    Ok(out)
}

/// Port of Symfony 8.1 `resolveVariables` (the documented regex, hand-lexed —
/// regex-lite has no lookbehind). External non-loaded values get their
/// backslashes doubled and `$` marked literal before substitution, so secrets
/// from the process env survive the final unescape byte-for-byte.
fn resolve_variables_text(
    value: &str,
    lookup: &mut dyn EnvLookup,
    mkerr: &dyn Fn(&str) -> DcdError,
) -> Result<String> {
    if !value.contains('$') {
        return Ok(value.to_string());
    }
    let bytes = value.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' && bytes[index] != b'$' {
            let ch = value[index..].chars().next().unwrap();
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }

        let run_start = index;
        while index < bytes.len() && bytes[index] == b'\\' {
            index += 1;
        }
        let backslashes = index - run_start;
        if index >= bytes.len() || bytes[index] != b'$' {
            out.push_str(&value[run_start..index]);
            continue;
        }
        if index + 1 < bytes.len() && bytes[index + 1] == b'(' {
            out.push_str(&value[run_start..=index]);
            index += 1;
            continue;
        }

        let reference = parse_var_reference(bytes, index + 1);
        if backslashes % 2 == 1 {
            out.push_str(&value[run_start + 1..reference.end]);
            index = reference.end;
            continue;
        }
        let Some(name) = reference.name else {
            out.push_str(&value[run_start..reference.end]);
            index = reference.end;
            continue;
        };
        if reference.opening_brace && !reference.closing_brace {
            return Err(mkerr("Unclosed braces on variable expansion"));
        }

        let looked_up = lookup.look_up(&name);
        let mut resolved = looked_up.value;
        if !resolved.is_empty() && !looked_up.loaded {
            if looked_up.external {
                resolved = resolved.replace('\\', "\\\\");
            }
            resolved = resolved.replace('$', "\0");
        }
        if resolved.is_empty() {
            if let Some(default) = &reference.default {
                if let Some(unsupported) = default.chars().find(|c| "'\"{$".contains(*c)) {
                    return Err(mkerr(&format!(
                        "Unsupported character \"{unsupported}\" found in the default value of variable \"${name}\"."
                    )));
                }
                resolved = default[2..].to_string();
                if default.as_bytes()[1] == b'=' {
                    lookup.assign(&name, &resolved);
                }
            }
        }
        if !reference.opening_brace && reference.closing_brace {
            resolved.push('}');
        }
        out.push_str(&value[run_start..run_start + backslashes]);
        out.push_str(&resolved);
        index = reference.end;
    }
    Ok(out)
}

struct VarReference {
    end: usize,
    opening_brace: bool,
    closing_brace: bool,
    name: Option<String>,
    default: Option<String>,
}

/// Lexes `\{?(_*[A-Za-z][A-Za-z0-9_]*)?(:[-=][^}]*)?\}?` starting after the `$`.
fn parse_var_reference(bytes: &[u8], after_dollar: usize) -> VarReference {
    let mut index = after_dollar;
    let opening_brace = index < bytes.len() && bytes[index] == b'{';
    if opening_brace {
        index += 1;
    }

    let name_start = index;
    let mut probe = index;
    while probe < bytes.len() && bytes[probe] == b'_' {
        probe += 1;
    }
    let name = if probe < bytes.len() && bytes[probe].is_ascii_alphabetic() {
        probe += 1;
        while probe < bytes.len() && (bytes[probe].is_ascii_alphanumeric() || bytes[probe] == b'_') {
            probe += 1;
        }
        index = probe;
        Some(String::from_utf8_lossy(&bytes[name_start..probe]).into_owned())
    } else {
        None
    };

    let default_start = index;
    let default = if index + 1 < bytes.len()
        && bytes[index] == b':'
        && matches!(bytes[index + 1], b'-' | b'=')
    {
        index += 2;
        while index < bytes.len() && bytes[index] != b'}' {
            index += 1;
        }
        Some(String::from_utf8_lossy(&bytes[default_start..index]).into_owned())
    } else {
        None
    };

    let closing_brace = index < bytes.len() && bytes[index] == b'}';
    if closing_brace {
        index += 1;
    }

    VarReference {
        end: index,
        opening_brace,
        closing_brace,
        name,
        default,
    }
}

/// A newline inside the parens still balances — quoted values carry real
/// newlines into raw values, and Symfony's command regex matches them, so the
/// refusal must fire there too (never a silent literal).
fn balanced_paren_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn escape_newlines(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\n', "\\n")
}

// ---------- chain loading (spec §5.2.1 / §5.2.2) ----------

#[derive(Debug)]
pub struct EnvLayer {
    pub label: String,
    /// This layer's keys, mapped to their final chain-resolved values.
    pub values: IndexMap<String, String>,
}

#[derive(Debug)]
pub struct ResolvedEnv {
    pub layers: Vec<EnvLayer>,
    pub skipped: Vec<PathBuf>,
    pub container_env: BTreeMap<String, String>,
    pub interpolation_env: HashMap<String, String>,
    pub shadowed_keys: Vec<String>,
}

/// Loads the chain hanging off `base` (Symfony `loadEnv` semantics): `<base>`
/// (or `<base>.dist`) → `<base>.local` → `<base>.<stage>` → `<base>.<stage>.local`.
/// `base_required` makes a missing base file an error — used when the operator
/// pointed dcd at an explicit `--env-file`. A `None` base means the operator
/// named no file source at all (`--env-stdin` alone, spec §5.2.1): dcd discovers
/// nothing on disk and the stdin document is the whole chain.
pub fn resolve(
    base: Option<&Path>,
    stage: &str,
    base_required: bool,
    stdin_document: Option<&str>,
    process_env: &HashMap<String, String>,
) -> Result<ResolvedEnv> {
    let Some(base) = base else {
        let documents: Vec<(String, String)> = stdin_document
            .map(|doc| vec![("<stdin>".to_string(), doc.to_string())])
            .unwrap_or_default();
        return resolve_documents(&documents, process_env, Vec::new());
    };

    let parent = match base.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !parent.is_dir() {
        return Err(DcdError::Config(format!(
            "env dir {} does not exist",
            parent.display()
        )));
    }

    let mut documents: Vec<(String, String)> = Vec::new();
    let mut skipped: Vec<PathBuf> = Vec::new();

    let dist = suffixed(base, ".dist");
    if is_present(base)? {
        documents.push((display_of(base), read_env_file(base)?));
    } else if is_present(&dist)? {
        skipped.push(base.to_path_buf());
        documents.push((display_of(&dist), read_env_file(&dist)?));
    } else if base_required {
        return Err(DcdError::Config(format!(
            "env file {} does not exist (checked {} too)",
            base.display(),
            dist.display()
        )));
    } else {
        skipped.push(base.to_path_buf());
        skipped.push(dist);
    }

    let mut optional = vec![suffixed(base, ".local")];
    if !stage.is_empty() && stage != "local" {
        optional.push(suffixed(base, &format!(".{stage}")));
        optional.push(suffixed(base, &format!(".{stage}.local")));
    }
    for path in optional {
        if is_present(&path)? {
            documents.push((display_of(&path), read_env_file(&path)?));
        } else {
            skipped.push(path);
        }
    }
    if let Some(doc) = stdin_document {
        documents.push(("<stdin>".to_string(), doc.to_string()));
    }

    resolve_documents(&documents, process_env, skipped)
}

fn suffixed(base: &Path, suffix: &str) -> PathBuf {
    let mut name = base.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// The pure core of `resolve` — Symfony `doLoad` + `resolveLoadedVars` over the
/// layered documents, the reserved-key guard, and the two maps — separated from
/// file discovery so unit tests need no filesystem. Process env wins: a key the
/// environment already defines keeps its external value; everything else is
/// loaded raw and resolved deferred (≤5 passes), so later layers can override
/// variables referenced by earlier ones and forward references resolve.
pub fn resolve_documents(
    documents: &[(String, String)],
    process_env: &HashMap<String, String>,
    skipped: Vec<PathBuf>,
) -> Result<ResolvedEnv> {
    let mut working: IndexMap<String, String> = IndexMap::new();
    let mut sorted_process: Vec<(&String, &String)> = process_env.iter().collect();
    sorted_process.sort();
    for (key, value) in sorted_process {
        working.insert(key.clone(), value.clone());
    }

    let mut loaded: HashSet<String> = HashSet::new();
    let mut overridden: HashMap<String, String> = HashMap::new();
    let mut loaded_raw: Vec<String> = Vec::new();
    let mut loaded_raw_set: HashSet<String> = HashSet::new();
    let mut chain_keys: Vec<String> = Vec::new();
    let mut chain_key_set: HashSet<String> = HashSet::new();
    let mut layer_keys: Vec<(String, Vec<String>)> = Vec::new();

    for (label, data) in documents {
        let values = Parser::new(data, label).parse_raw()?;
        for key in values.keys() {
            if let Some(reason) = reserved_key_reason(key, ReservedAllowance::None) {
                return Err(DcdError::Config(format!(
                    "{key} in {label} is reserved ({reason}); set it via release.run.env if a container needs it"
                )));
            }
        }
        layer_keys.push((label.clone(), values.keys().cloned().collect()));
        for (name, value) in values {
            if chain_key_set.insert(name.clone()) {
                chain_keys.push(name.clone());
            }
            let already_defined = working.contains_key(&name);
            if already_defined && !overridden.contains_key(&name) {
                overridden.insert(name.clone(), working[&name].clone());
            }
            if loaded.contains(&name) || !already_defined {
                if loaded_raw_set.insert(name.clone()) {
                    loaded_raw.push(name.clone());
                }
                working.insert(name.clone(), value);
                loaded.insert(name);
            }
        }
    }

    resolve_loaded_vars(&mut working, &loaded, &overridden, &loaded_raw)?;

    let mut container_env = BTreeMap::new();
    let mut shadowed_keys = Vec::new();
    for key in &chain_keys {
        if process_env.contains_key(key) {
            shadowed_keys.push(key.clone());
        }
        let value = working.get(key).cloned().unwrap_or_default();
        container_env.insert(key.clone(), value);
    }
    shadowed_keys.sort();

    let layers = layer_keys
        .into_iter()
        .map(|(label, keys)| EnvLayer {
            label,
            values: keys
                .into_iter()
                .map(|key| {
                    let value = working.get(&key).cloned().unwrap_or_default();
                    (key, value)
                })
                .collect(),
        })
        .collect();

    let interpolation_env: HashMap<String, String> = working.into_iter().collect();

    Ok(ResolvedEnv {
        layers,
        skipped,
        container_env,
        interpolation_env,
        shadowed_keys,
    })
}

/// Port of Symfony 8.1 `resolveLoadedVars`: resolve the raw chain values in
/// place, up to 5 passes until a fixpoint, then restore literal-`$` markers and
/// backslash escapes. Self-referencing variables hide their own raw value so
/// the pre-chain external value (or the default operator) applies instead.
fn resolve_loaded_vars(
    working: &mut IndexMap<String, String>,
    loaded: &HashSet<String>,
    overridden: &HashMap<String, String>,
    loaded_raw: &[String],
) -> Result<()> {
    let mut self_referencing: HashSet<String> = HashSet::new();
    for name in loaded_raw {
        let value = working.get(name).cloned().unwrap_or_default();
        if is_self_referencing(name, &value) {
            self_referencing.insert(name.clone());
        }
    }

    let mut assigned: IndexMap<String, String> = IndexMap::new();
    let mut unresolved_after_final_pass: Vec<String> = Vec::new();
    for _pass in 0..5 {
        let mut resolved: IndexMap<String, String> = IndexMap::new();
        for name in loaded_raw {
            let value = working.get(name).cloned().unwrap_or_default();
            if !value.contains('$') {
                continue;
            }

            let hides_own_value = self_referencing.contains(name);
            let own_backup = working.get(name).cloned();
            if hides_own_value {
                match overridden.get(name) {
                    Some(external) => {
                        working.insert(name.clone(), protect_overridden(external));
                    }
                    None => {
                        working.shift_remove(name);
                    }
                }
            }

            let mkerr = |message: &str| {
                DcdError::Config(format!("{message} while resolving {name} from the env chain"))
            };
            let result = resolve_commands_text(&value, &mkerr).and_then(|expanded| {
                let mut lookup = ChainLookup {
                    working,
                    loaded,
                    assigned: &mut assigned,
                };
                resolve_variables_text(&expanded, &mut lookup, &mkerr)
            });

            if hides_own_value {
                match own_backup {
                    Some(backup) => {
                        working.insert(name.clone(), backup);
                    }
                    None => {
                        working.shift_remove(name);
                    }
                }
            }

            let resolved_value = result?;
            if resolved_value != value {
                resolved.insert(name.clone(), resolved_value);
            }
        }
        if resolved.is_empty() {
            unresolved_after_final_pass.clear();
            break;
        }
        unresolved_after_final_pass = resolved.keys().cloned().collect();
        for (name, value) in resolved {
            working.insert(name, value);
        }
    }
    if !unresolved_after_final_pass.is_empty() {
        return Err(DcdError::Config(format!(
            "Too many levels of variable indirection in env vars: {}.",
            unresolved_after_final_pass.join(", ")
        )));
    }

    for name in loaded_raw {
        if let Some(value) = working.get(name) {
            let restored = restore_literal_markers(value);
            if &restored != value {
                working.insert(name.clone(), restored);
            }
        }
    }
    Ok(())
}

/// Symfony `isSelfReferencing`: does a raw value reference the variable it
/// defines (`MY_VAR=${MY_VAR:-default}`)? Escaped dollars are already `\x00`
/// markers in raw values, so only live references match.
fn is_self_referencing(name: &str, value: &str) -> bool {
    if !value.contains('$') {
        return false;
    }
    let bytes = value.as_bytes();
    let name_bytes = name.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let mut probe = index + 1;
        if probe < bytes.len() && bytes[probe] == b'{' {
            probe += 1;
        }
        if bytes[probe..].starts_with(name_bytes) {
            let after = probe + name_bytes.len();
            let at_boundary = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            if at_boundary {
                return true;
            }
        }
        index += 1;
    }
    false
}

/// Symfony's self-reference swap protection: double escaped backslash pairs and
/// mark `$` literal before temporarily substituting the overridden value.
fn protect_overridden(value: &str) -> String {
    value.replace("\\\\", "\\\\\\\\").replace('$', "\0")
}

/// The final restore, in Symfony's order: markers back to `$` first, then
/// escaped backslashes collapse.
fn restore_literal_markers(value: &str) -> String {
    value.replace('\0', "$").replace("\\\\", "\\")
}

fn display_of(path: &Path) -> String {
    path.display().to_string()
}

/// Only a confirmed-absent file is skipped; a dangling symlink or an unstattable
/// path is loud (spec §5.2.1 — a silently skipped layer deploys the wrong env).
fn is_present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(DcdError::Config(format!(
            "cannot stat env file {}: {e}",
            path.display()
        ))),
    }
}

fn read_env_file(path: &Path) -> Result<String> {
    if path.is_dir() {
        return Err(DcdError::Config(format!(
            "env file {} is a directory",
            path.display()
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| DcdError::Config(format!("cannot read env file {}: {e}", path.display())))?;
    String::from_utf8(bytes)
        .map_err(|_| DcdError::Config(format!("env file {} is not valid UTF-8", path.display())))
}

// ---------- reserved keys (spec §5.2.3) ----------

/// What an explicit config map is allowed to set that the chain is not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReservedAllowance {
    /// Chain layers: nothing reserved is allowed.
    None,
    /// `compose.env`: `COMPOSE_*` is deliberate configuration.
    ComposeVars,
    /// `release.run.env` / `workers.template.env`: containers may need proxies.
    ProxyVars,
}

pub fn reserved_key_reason(key: &str, allowance: ReservedAllowance) -> Option<&'static str> {
    let is_proxy = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"]
        .iter()
        .any(|p| key.eq_ignore_ascii_case(p));
    if is_proxy {
        if allowance == ReservedAllowance::ProxyVars {
            return None;
        }
        return Some("configures dcd's own tooling");
    }
    if key.starts_with("COMPOSE_") {
        if allowance == ReservedAllowance::ComposeVars {
            return None;
        }
        return Some("configures dcd's own tooling");
    }
    if key == "PATH"
        || key == "HOME"
        || key.starts_with("LD_")
        || key.starts_with("DOCKER_")
        || key.starts_with("BUILDX_")
    {
        return Some("configures dcd's own tooling");
    }
    None
}

// ---------- per-container filters (spec §5.2.4) ----------

/// The one producer of a container class's delivered key set (spec §5.2.4):
/// chain keys through the filters, unioned with the explicit map's keys,
/// sorted and deduplicated. `check` and the engine both call this.
pub fn delivered_keys<'k>(
    container_env: &BTreeMap<String, String>,
    include: &[String],
    exclude: &[String],
    explicit_keys: impl Iterator<Item = &'k String>,
) -> Result<Vec<String>> {
    let filtered = filter_container_keys(container_env, include, exclude)?;
    let mut union: std::collections::BTreeSet<String> = filtered.into_iter().collect();
    union.extend(explicit_keys.cloned());
    Ok(union.into_iter().collect())
}

/// Explicit config env maps bypass the dotenv lexer, so their keys get the same
/// charset rule here — a key with whitespace, `=`, or a newline would corrupt the
/// `-e` argv or the generated workers YAML (and is reachable from Lua `ctx.cfg`).
pub fn invalid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else { return true };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return true;
    }
    !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Applies `env_include`/`env_exclude` (full-match regexes, exclude wins) to the
/// container env and returns the surviving keys in sorted order.
pub fn filter_container_keys(
    container_env: &BTreeMap<String, String>,
    include: &[String],
    exclude: &[String],
) -> Result<Vec<String>> {
    let include_set = compile_full_match(include, "env_include")?;
    let exclude_set = compile_full_match(exclude, "env_exclude")?;

    let mut keys = Vec::new();
    for key in container_env.keys() {
        let included = include_set.is_empty() || include_set.iter().any(|r| r.is_match(key));
        let excluded = exclude_set.iter().any(|r| r.is_match(key));
        if included && !excluded {
            keys.push(key.clone());
        }
    }
    Ok(keys)
}

fn compile_full_match(patterns: &[String], owner: &str) -> Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(&format!("^(?:{pattern})$"))
                .map_err(|e| DcdError::Config(format!("{owner}: bad pattern `{pattern}`: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests;
