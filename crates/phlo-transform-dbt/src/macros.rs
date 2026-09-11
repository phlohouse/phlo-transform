//! Static helpers for Jinja-adjacent constructs.
//!
//! Everything here is provably static: project-macro definitions, integer
//! arithmetic in macro arguments, `{% set %}`/`{% for %}` evaluation over
//! literals, and the recognised package helpers that lower to native SQL.
//! Anything the parser cannot fully account for returns `None` and the
//! caller falls back to REVIEW — nothing executes.

use std::collections::BTreeMap;

use serde_yaml::Mapping;

use crate::jinja::{self, Call, LooseCall, Segment};
use crate::project::DbtSqlFile;
use crate::pylit::{self, Lit};
use crate::translate::yaml_to_lit;

/// Compile-time scope: names bound by `{% set %}` / `{% for %}`.
pub type Scope = BTreeMap<String, Lit>;

/// A `{% macro name(params) %}` definition, body kept as segments.
#[derive(Clone, Debug)]
pub struct MacroDef {
    /// Parameter names in declaration order.
    pub params: Vec<String>,
    /// Literal parameter defaults (`name, value`).
    pub defaults: Vec<(String, Lit)>,
    /// Segments between `{% macro %}` and the matching `{% endmacro %}`.
    pub body: Vec<Segment>,
}

/// Extract `{% macro name(...) %}...{% endmacro %}` definitions from a macro
/// file. Macros with unparseable signatures are skipped (they stay REVIEW
/// through the per-macro outcome).
pub fn parse_macro_defs(file: &DbtSqlFile) -> BTreeMap<String, MacroDef> {
    let segments = jinja::scan(&file.sql);
    let mut defs = BTreeMap::new();
    let mut index = 0usize;
    while index < segments.len() {
        if let Segment::Stmt { inner, .. } = &segments[index] {
            if let Some(rest) = inner.strip_prefix("macro ") {
                if let Some((name, def, end)) = parse_def(rest, &segments, index) {
                    defs.insert(name, def);
                    index = end;
                }
            }
        }
        index += 1;
    }
    defs
}

fn parse_def(rest: &str, segments: &[Segment], index: usize) -> Option<(String, MacroDef, usize)> {
    let open = rest.find('(')?;
    let close = rest.rfind(')')?;
    if close <= open {
        return None;
    }
    let name = rest[..open].trim().to_string();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
    {
        return None;
    }
    let (positional, keyword) = pylit::parse_args(&rest[open + 1..close]).ok()?;
    let mut params = Vec::new();
    for value in positional {
        match value {
            Lit::Ident(param) => params.push(param),
            _ => return None,
        }
    }
    let mut defaults = Vec::new();
    for (key, value) in keyword {
        params.push(key.clone());
        defaults.push((key, value));
    }
    // Macros cannot nest; the next `{% endmacro %}` closes this one.
    for end in index + 1..segments.len() {
        if let Segment::Stmt { inner, .. } = &segments[end] {
            if jinja::stmt_keyword(inner) == "endmacro" {
                return Some((
                    name,
                    MacroDef {
                        params,
                        defaults,
                        body: segments[index + 1..end].to_vec(),
                    },
                    end,
                ));
            }
        }
    }
    None
}

/// Either a strictly-parsed call or a loose (raw-text) one.
#[derive(Clone, Copy)]
pub enum ArgsRef<'a> {
    Strict(&'a Call),
    Loose(&'a LooseCall),
}

impl<'a> ArgsRef<'a> {
    /// Keyword or positional argument as a literal, when parseable.
    pub fn get(&self, position: usize, name: &str) -> Option<Lit> {
        match self {
            ArgsRef::Strict(call) => call
                .kwarg(name)
                .or_else(|| call.positional.get(position))
                .cloned(),
            ArgsRef::Loose(_) => self
                .get_raw(position, name)
                .and_then(|text| pylit::parse_value(&text).ok()),
        }
    }

    /// Names of every keyword argument supplied to the call.
    pub fn kwarg_names(&self) -> Vec<&str> {
        match self {
            ArgsRef::Strict(call) => call.keyword.iter().map(|(name, _)| name.as_str()).collect(),
            ArgsRef::Loose(call) => call
                .args
                .iter()
                .filter_map(|(key, _)| key.as_deref())
                .collect(),
        }
    }

    /// Keyword or positional argument's raw text (loose calls keep it
    /// verbatim; strict calls render the literal back).
    pub fn get_raw(&self, position: usize, name: &str) -> Option<String> {
        match self {
            ArgsRef::Strict(call) => call
                .kwarg(name)
                .or_else(|| call.positional.get(position))
                .and_then(render_lit),
            ArgsRef::Loose(call) => call
                .args
                .iter()
                .find(|(key, _)| key.as_deref() == Some(name))
                .or_else(|| call.args.get(position).filter(|(k, _)| k.is_none()))
                .map(|(_, text)| text.trim().to_string()),
        }
    }
}

/// Render a literal the way dbt emits it: strings verbatim (quoting is the
/// caller's concern), numbers plainly. Returns `None` for compounds that
/// cannot appear inline.
pub fn render_lit(value: &Lit) -> Option<String> {
    match value {
        Lit::Str(text) | Lit::Ident(text) => Some(text.clone()),
        Lit::Int(number) => Some(number.to_string()),
        Lit::Float(number) => Some(number.to_string()),
        Lit::Bool(flag) => Some(flag.to_string()),
        Lit::None => Some("null".to_string()),
        Lit::List(_) | Lit::Dict(_) | Lit::Jinja(_) => None,
    }
}

/// Evaluate a `{% set %}` / `{% for %}` right-hand side statically:
/// Python literals, names bound in `scope`, `var()` lookups, a small set of
/// Jinja filters (`length`, `replace`, `default`, case/`trim`, `join`,
/// `int`/`string`) and small integer arithmetic. Returns `None` for
/// anything dynamic.
pub fn eval_static(text: &str, scope: &Scope, vars: &Mapping) -> Option<Lit> {
    let text = text.trim();
    // `expr | filter | filter(...)` — top-level pipes only.
    let parts = split_top_level(text, b'|');
    if parts.len() > 1 {
        let mut value = eval_static(&parts[0], scope, vars)?;
        for filter in &parts[1..] {
            value = apply_filter(value, filter.trim(), scope, vars)?;
        }
        return Some(value);
    }
    if let Ok(lit) = pylit::parse_value(text) {
        // `var('x')` parses as `Ident("var(...)")` — on a scope miss, fall
        // through to the call/int evaluators rather than failing.
        if let Some(resolved) = resolve_scope(lit, scope) {
            return Some(resolved);
        }
    }
    if let Some(call) = jinja::parse_call(text) {
        if call.name == "var" {
            return eval_var(&call, vars, scope);
        }
        return None;
    }
    eval_int_expr(text).map(Lit::Int)
}

/// Split on a top-level byte (quotes and `()[]{}` are not split points).
pub fn split_top_level(input: &str, needle: u8) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut quote = None;
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => match quote {
                Some(q) if q == bytes[i] => quote = None,
                None => quote = Some(bytes[i]),
                _ => {}
            },
            b'\\' if quote.is_some() => i += 1,
            b'(' | b'[' | b'{' if quote.is_none() => depth += 1,
            b')' | b']' | b'}' if quote.is_none() => depth = depth.saturating_sub(1),
            c if c == needle && quote.is_none() && depth == 0 => {
                parts.push(input[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(input[start..].to_string());
    parts
}

/// Find a top-level word (` in `, ` if `, ` or `, ...) — the needle must be
/// bounded by spaces and outside quotes/brackets. Returns the byte offset.
pub fn find_top_level_word(input: &str, needle: &str) -> Option<usize> {
    let bytes = input.as_bytes();
    let n = needle.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut i = 0usize;
    while i + n.len() <= bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => match quote {
                Some(q) if q == bytes[i] => quote = None,
                None => quote = Some(bytes[i]),
                _ => {}
            },
            b'\\' if quote.is_some() => i += 1,
            b'(' | b'[' | b'{' if quote.is_none() => depth += 1,
            b')' | b']' | b'}' if quote.is_none() => depth = depth.saturating_sub(1),
            _ => {}
        }
        if quote.is_none() && depth == 0 && bytes[i..].starts_with(n) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Apply one recognised Jinja filter (`name` optionally with `(...)` args).
/// Unknown filters return `None` — the whole expression then stays REVIEW.
fn apply_filter(value: Lit, filter: &str, scope: &Scope, vars: &Mapping) -> Option<Lit> {
    let (name, args) = match filter.find('(') {
        Some(open) => {
            let close = filter.rfind(')')?;
            let (positional, keyword) = pylit::parse_args(&filter[open + 1..close]).ok()?;
            if !keyword.is_empty() {
                return None;
            }
            let args = positional
                .into_iter()
                .map(|lit| resolve_scope(lit, scope))
                .collect::<Option<Vec<Lit>>>()?;
            (filter[..open].trim(), args)
        }
        None => (filter.trim(), Vec::new()),
    };
    let _ = vars;
    let text = |lit: &Lit| lit.as_str().map(str::to_string);
    match name {
        "length" | "count" => match value {
            Lit::Str(s) | Lit::Ident(s) => Some(Lit::Int(s.len() as i64)),
            Lit::List(items) => Some(Lit::Int(items.len() as i64)),
            Lit::Dict(items) => Some(Lit::Int(items.len() as i64)),
            _ => None,
        },
        "replace" if args.len() == 2 => {
            let (old, new) = (text(&args[0])?, text(&args[1])?);
            Some(Lit::Str(text(&value)?.replace(&old, &new)))
        }
        "default" | "d" if args.len() == 1 => match value {
            Lit::None => Some(args.into_iter().next().unwrap()),
            other => Some(other),
        },
        "lower" => text(&value).map(|s| Lit::Str(s.to_lowercase())),
        "upper" => text(&value).map(|s| Lit::Str(s.to_uppercase())),
        "trim" | "strip" => text(&value).map(|s| Lit::Str(s.trim().to_string())),
        "join" if args.len() == 1 => {
            let sep = text(&args[0])?;
            let items = match value {
                Lit::List(items) => items,
                _ => return None,
            };
            let parts: Option<Vec<String>> = items.iter().map(&text).collect();
            Some(Lit::Str(parts?.join(&sep)))
        }
        "int" => match &value {
            Lit::Int(n) => Some(Lit::Int(*n)),
            Lit::Float(f) => Some(Lit::Int(*f as i64)),
            Lit::Str(s) | Lit::Ident(s) => s.trim().parse().ok().map(Lit::Int),
            _ => None,
        },
        "string" => match &value {
            Lit::Str(_) | Lit::Ident(_) => Some(value),
            Lit::Int(n) => Some(Lit::Str(n.to_string())),
            Lit::Bool(b) => Some(Lit::Str(b.to_string())),
            _ => None,
        },
        "e" | "escape" => text(&value).map(|s| Lit::Str(s.replace('\'', "''"))),
        "list" => Some(value),
        _ => None,
    }
}

/// `var('name'[, default])` against project `vars:`. The name argument may
/// itself be a scope-bound identifier (`var(param, default)` inside a macro).
fn eval_var(call: &Call, vars: &Mapping, scope: &Scope) -> Option<Lit> {
    let name = call
        .positional
        .first()
        .cloned()
        .and_then(|lit| resolve_scope(lit, scope))
        .and_then(|lit| lit.as_str().map(str::to_string))?;
    let value = vars
        .get(serde_yaml::Value::String(name.to_string()))
        .map(yaml_to_lit);
    value.or_else(|| {
        call.positional
            .get(1)
            .cloned()
            .and_then(|lit| resolve_scope(lit, scope))
    })
}

/// Substitute scope-bound identifiers inside a literal. Dotted names first
/// match a flat scope key (`loop.index`, `target.type`), then walk dict
/// values (`country.country_name` over a `country` loop variable).
fn resolve_scope(lit: Lit, scope: &Scope) -> Option<Lit> {
    fn attr<'a>(mut value: &'a Lit, rest: &str) -> Option<&'a Lit> {
        for part in rest.split('.') {
            match value {
                Lit::Dict(items) => {
                    value = items.iter().find(|(k, _)| k == part).map(|(_, v)| v)?
                }
                _ => return None,
            }
        }
        Some(value)
    }
    match lit {
        Lit::Ident(name) => scope.get(&name).cloned().or_else(|| {
            let (head, rest) = name.split_once('.')?;
            attr(scope.get(head)?, rest).cloned()
        }),
        Lit::List(items) => Some(Lit::List(
            items
                .into_iter()
                .map(|item| resolve_scope(item, scope))
                .collect::<Option<Vec<Lit>>>()?,
        )),
        Lit::Dict(items) => Some(Lit::Dict(
            items
                .into_iter()
                .map(|(key, value)| resolve_scope(value, scope).map(|value| (key, value)))
                .collect::<Option<Vec<(String, Lit)>>>()?,
        )),
        other => Some(other),
    }
}

/// Small integer arithmetic evaluator for macro arguments such as
/// `n_dateparts = 365 * 10`. Accepts digits, `+ - * / %`, parentheses and
/// whitespace only — anything else is `None`.
pub fn eval_int_expr(text: &str) -> Option<i64> {
    if text.is_empty()
        || !text.chars().all(|c| {
            c.is_ascii_digit() || matches!(c, '+' | '-' | '*' | '/' | '%' | '(' | ')' | ' ' | '\t')
        })
    {
        return None;
    }
    let mut parser = IntParser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let value = parser.expr()?;
    parser.skip_ws();
    (parser.pos == text.len()).then_some(value)
}

struct IntParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> IntParser<'a> {
    fn skip_ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    fn expr(&mut self) -> Option<i64> {
        let mut value = self.term()?;
        loop {
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b'+') => {
                    self.pos += 1;
                    value = value.checked_add(self.term()?)?;
                }
                Some(b'-') => {
                    self.pos += 1;
                    value = value.checked_sub(self.term()?)?;
                }
                _ => return Some(value),
            }
        }
    }

    fn term(&mut self) -> Option<i64> {
        let mut value = self.factor()?;
        loop {
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b'*') => {
                    self.pos += 1;
                    value = value.checked_mul(self.factor()?)?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    let rhs = self.factor()?;
                    value = value.checked_div(rhs)?;
                }
                Some(b'%') => {
                    self.pos += 1;
                    let rhs = self.factor()?;
                    value = value.checked_rem(rhs)?;
                }
                _ => return Some(value),
            }
        }
    }

    fn factor(&mut self) -> Option<i64> {
        self.skip_ws();
        match self.bytes.get(self.pos) {
            Some(b'(') => {
                self.pos += 1;
                let value = self.expr()?;
                self.skip_ws();
                (self.bytes.get(self.pos) == Some(&b')')).then(|| {
                    self.pos += 1;
                    value
                })
            }
            Some(b'-') => {
                self.pos += 1;
                self.factor().map(|v| -v)
            }
            Some(b'+') => {
                self.pos += 1;
                self.factor()
            }
            Some(c) if c.is_ascii_digit() => {
                let start = self.pos;
                while matches!(self.bytes.get(self.pos), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .ok()?
                    .parse()
                    .ok()
            }
            _ => None,
        }
    }
}

/// Evaluate a static `{% if %}` condition. Supports booleans, `not`,
/// `==`/`!=` against literals, and truthiness of scope-bound values
/// (`loop.last`, set variables). Anything else is `None`.
pub fn eval_condition(text: &str, scope: &Scope, vars: &Mapping) -> Option<bool> {
    eval_condition_with(text, scope, vars, &mut |t, s, v| eval_static(t, s, v))
}

/// `eval_condition` with a caller-supplied leaf evaluator — used by the
/// translator to let conditions call statically-renderable project macros
/// (e.g. a `boolean_var('x')` wrapper around `var`).
pub fn eval_condition_with(
    text: &str,
    scope: &Scope,
    vars: &Mapping,
    leaf: &mut dyn FnMut(&str, &Scope, &Mapping) -> Option<Lit>,
) -> Option<bool> {
    let mut text = text.trim();
    // Strip outer parentheses only when they wrap the whole expression:
    // `(a and b)` → `a and b`, but `(a) and (b)` is left alone.
    while text.starts_with('(') && text.ends_with(')') {
        let mut depth = 0i32;
        let mut wraps = true;
        let last = text.len() - 1;
        for (i, c) in text.char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth <= 0 && i != last {
                        wraps = false;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !wraps || depth != 0 {
            break;
        }
        text = text[1..text.len() - 1].trim();
    }
    if let Some(rest) = text.strip_prefix("not ") {
        return eval_condition_with(rest, scope, vars, leaf).map(|v| !v);
    }
    // `a or b`, `a and b` — top-level word operators.
    if let Some(position) = find_top_level_word(text, " or ") {
        let (left, right) = text.split_at(position);
        let right = &right[" or ".len()..];
        return Some(
            eval_condition_with(left, scope, vars, leaf)?
                || eval_condition_with(right, scope, vars, leaf)?,
        );
    }
    if let Some(position) = find_top_level_word(text, " and ") {
        let (left, right) = text.split_at(position);
        let right = &right[" and ".len()..];
        return Some(
            eval_condition_with(left, scope, vars, leaf)?
                && eval_condition_with(right, scope, vars, leaf)?,
        );
    }
    // `x in (a, b)` / `x not in (a, b)` — membership over a static list.
    for needle in [" not in ", " in "] {
        if let Some(position) = find_top_level_word(text, needle) {
            let (left, right) = text.split_at(position);
            let right = &right[needle.len()..];
            let member = leaf(left.trim(), scope, vars)?;
            let Some(Lit::List(items)) = leaf(right.trim(), scope, vars) else {
                return None;
            };
            let found = items.iter().any(|item| lit_eq(&member, item));
            return Some(if needle == " in " { found } else { !found });
        }
    }
    // `x is sameas y` / `x is not sameas y` / `x is [not] none`.
    for (needle, negate) in [(" is not sameas ", true), (" is sameas ", false)] {
        if let Some(position) = find_top_level_word(text, needle) {
            let (left, right) = text.split_at(position);
            let right = &right[needle.len()..];
            let equal = lit_eq(
                &leaf(left.trim(), scope, vars)?,
                &leaf(right.trim(), scope, vars)?,
            );
            return Some(if negate { !equal } else { equal });
        }
    }
    for (needle, negate) in [(" is not none", true), (" is none", false)] {
        if let Some(rest) = text.strip_suffix(needle) {
            let is_none = matches!(leaf(rest.trim(), scope, vars)?, Lit::None);
            return Some(if negate { !is_none } else { is_none });
        }
    }
    for op in ["==", "!=", ">=", "<=", ">", "<"] {
        if let Some(position) = text.find(op) {
            let (left, right) = text.split_at(position);
            let right = &right[op.len()..];
            let left = leaf(left.trim(), scope, vars)?;
            let right = leaf(right.trim(), scope, vars)?;
            return Some(match op {
                "==" => lit_eq(&left, &right),
                "!=" => !lit_eq(&left, &right),
                _ => {
                    let (Lit::Int(a), Lit::Int(b)) = (left, right) else {
                        return None;
                    };
                    match op {
                        ">=" => a >= b,
                        "<=" => a <= b,
                        ">" => a > b,
                        "<" => a < b,
                        _ => unreachable!(),
                    }
                }
            });
        }
    }
    leaf(text, scope, vars).map(|lit| match lit {
        Lit::Bool(b) => b,
        Lit::Int(n) => n != 0,
        Lit::Str(s) | Lit::Ident(s) => !s.is_empty(),
        Lit::List(items) => !items.is_empty(),
        Lit::Dict(items) => !items.is_empty(),
        Lit::None => false,
        Lit::Jinja(_) | Lit::Float(_) => false,
    })
}

fn lit_eq(left: &Lit, right: &Lit) -> bool {
    match (left, right) {
        (Lit::Str(a) | Lit::Ident(a), Lit::Str(b) | Lit::Ident(b)) => a == b,
        (Lit::Int(a), Lit::Int(b)) => a == b,
        (Lit::Bool(a), Lit::Bool(b)) => a == b,
        (Lit::None, Lit::None) => true,
        _ => left == right,
    }
}
