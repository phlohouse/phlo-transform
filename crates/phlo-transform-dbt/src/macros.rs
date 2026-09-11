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
/// Python literals, names bound in `scope`, `var()` lookups and small
/// integer arithmetic. Returns `None` for anything dynamic.
pub fn eval_static(text: &str, scope: &Scope, vars: &Mapping) -> Option<Lit> {
    let text = text.trim();
    if let Ok(lit) = pylit::parse_value(text) {
        return resolve_scope(lit, scope);
    }
    if let Some(call) = jinja::parse_call(text) {
        if call.name == "var" {
            return eval_var(&call, vars);
        }
        return None;
    }
    eval_int_expr(text).map(Lit::Int)
}

/// `var('name'[, default])` against project `vars:`.
fn eval_var(call: &Call, vars: &Mapping) -> Option<Lit> {
    let name = call.arg(0)?;
    let value = vars
        .get(serde_yaml::Value::String(name.to_string()))
        .map(yaml_to_lit);
    value.or_else(|| call.positional.get(1).cloned())
}

/// Substitute scope-bound identifiers inside a literal.
fn resolve_scope(lit: Lit, scope: &Scope) -> Option<Lit> {
    match lit {
        Lit::Ident(name) => scope.get(&name).cloned(),
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
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("not ") {
        return eval_condition(rest, scope, vars).map(|v| !v);
    }
    for op in ["==", "!="] {
        if let Some(position) = text.find(op) {
            let (left, right) = text.split_at(position);
            let right = &right[op.len()..];
            let left = eval_static(left, scope, vars)?;
            let right = eval_static(right, scope, vars)?;
            let equal = lit_eq(&left, &right);
            return Some(if op == "==" { equal } else { !equal });
        }
    }
    eval_static(text, scope, vars).map(|lit| match lit {
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
