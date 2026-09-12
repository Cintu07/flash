//! A small formula language, and the dependency graph that makes recalc incremental.
//!
//! The evaluator is a straightforward recursive descent parser over a fixed function set. What
//! matters for this project is not the expression language - every spreadsheet has one - but that
//! evaluation is driven by a *dependency graph*, so changing one cell recalculates the cells that
//! depend on it and nothing else (d7). That is the same claim the code adapter makes with test
//! selection and the doc adapter makes with pages, in the one domain where it is exact rather
//! than heuristic: a cell's dependencies are not an approximation.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Text(String),
    Bool(bool),
    Empty,
    /// A spreadsheet error, carrying its code: #DIV/0!, #REF!, #CYCLE!, #NAME?, #VALUE!.
    Error(String),
}

impl Value {
    pub fn as_number(&self) -> Result<f64, Value> {
        match self {
            Value::Number(n) => Ok(*n),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Value::Empty => Ok(0.0),
            Value::Text(t) => t.parse().map_err(|_| Value::Error("#VALUE!".into())),
            Value::Error(e) => Err(Value::Error(e.clone())),
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Value::Error(_))
    }

    pub fn render(&self) -> String {
        match self {
            Value::Number(n) => {
                if (n.fract()).abs() < f64::EPSILON {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            Value::Text(t) => t.clone(),
            Value::Bool(b) => b.to_string().to_uppercase(),
            Value::Empty => String::new(),
            Value::Error(e) => e.clone(),
        }
    }
}

/// `Sheet1!B4`, normalised.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellRef {
    pub sheet: String,
    pub col: u32,
    pub row: u32,
}

impl CellRef {
    pub fn parse(sheet_default: &str, text: &str) -> Option<CellRef> {
        let (sheet, rest) = match text.split_once('!') {
            Some((s, r)) => (s.trim_matches('\'').to_string(), r),
            None => (sheet_default.to_string(), text),
        };
        let rest = rest.replace('$', "");
        let letters: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let digits: String = rest[letters.len()..].to_string();
        if letters.is_empty() || digits.is_empty() {
            return None;
        }
        let mut col = 0u32;
        for c in letters.chars() {
            col = col * 26 + (c.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
        }
        Some(CellRef {
            sheet,
            col,
            row: digits.parse().ok()?,
        })
    }

    pub fn a1(&self) -> String {
        let mut col = self.col;
        let mut letters = String::new();
        while col > 0 {
            let rem = ((col - 1) % 26) as u8;
            letters.insert(0, (b'A' + rem) as char);
            col = (col - 1) / 26;
        }
        format!("{}!{}{}", self.sheet, letters, self.row)
    }
}

/// What a cell holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Literal(Value),
    Formula(String),
}

pub type Grid = BTreeMap<CellRef, Cell>;

/// Tokens the parser understands.
#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Ref(CellRef),
    Range(CellRef, CellRef),
    Op(char),
    Cmp(String),
    LParen,
    RParen,
    Comma,
}

fn tokenize(sheet: &str, src: &str, names: &HashMap<String, String>) -> Result<Vec<Tok>, String> {
    let bytes: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            ' ' | '\t' => i += 1,
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '+' | '-' | '*' | '/' | '^' | '&' => {
                out.push(Tok::Op(c));
                i += 1;
            }
            '<' | '>' | '=' => {
                let mut s = c.to_string();
                if i + 1 < bytes.len() && (bytes[i + 1] == '=' || bytes[i + 1] == '>') {
                    s.push(bytes[i + 1]);
                    i += 1;
                }
                out.push(Tok::Cmp(s));
                i += 1;
            }
            '"' => {
                let mut s = String::new();
                i += 1;
                while i < bytes.len() && bytes[i] != '"' {
                    s.push(bytes[i]);
                    i += 1;
                }
                i += 1;
                out.push(Tok::Str(s));
            }
            c if c.is_ascii_digit() || c == '.' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == '.') {
                    i += 1;
                }
                let text: String = bytes[start..i].iter().collect();
                out.push(Tok::Num(
                    text.parse().map_err(|_| format!("bad number {text}"))?,
                ));
            }
            c if c.is_ascii_alphabetic() || c == '_' || c == '$' || c == '\'' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || bytes[i] == '_'
                        || bytes[i] == '$'
                        || bytes[i] == '!'
                        || bytes[i] == '\'')
                {
                    i += 1;
                }
                let word: String = bytes[start..i].iter().collect();

                // A range?
                if i < bytes.len() && bytes[i] == ':' {
                    let mut j = i + 1;
                    while j < bytes.len()
                        && (bytes[j].is_ascii_alphanumeric() || bytes[j] == '$' || bytes[j] == '!')
                    {
                        j += 1;
                    }
                    let second: String = bytes[i + 1..j].iter().collect();
                    if let (Some(a), Some(b)) =
                        (CellRef::parse(sheet, &word), CellRef::parse(sheet, &second))
                    {
                        out.push(Tok::Range(a, b));
                        i = j;
                        continue;
                    }
                }

                // A defined name resolves to whatever it points at.
                if let Some(target) = names.get(&word) {
                    if let Some((a, b)) = target.split_once(':')
                        && let (Some(x), Some(y)) =
                            (CellRef::parse(sheet, a), CellRef::parse(sheet, b))
                    {
                        out.push(Tok::Range(x, y));
                        continue;
                    }
                    if let Some(r) = CellRef::parse(sheet, target) {
                        out.push(Tok::Ref(r));
                        continue;
                    }
                }

                match CellRef::parse(sheet, &word) {
                    Some(r) if i < bytes.len() && bytes[i] == '(' => {
                        // Something like `LOG10(` parses as a cell ref but is a function.
                        let _ = r;
                        out.push(Tok::Ident(word.to_ascii_uppercase()));
                    }
                    Some(r) => out.push(Tok::Ref(r)),
                    None => out.push(Tok::Ident(word.to_ascii_uppercase())),
                }
            }
            other => return Err(format!("unexpected character {other:?}")),
        }
    }
    Ok(out)
}

/// Every cell a formula reads, directly.
pub fn dependencies(
    sheet: &str,
    formula: &str,
    names: &HashMap<String, String>,
) -> Result<BTreeSet<CellRef>, String> {
    let toks = tokenize(sheet, formula.trim_start_matches('='), names)?;
    let mut out = BTreeSet::new();
    for t in toks {
        match t {
            Tok::Ref(r) => {
                out.insert(r);
            }
            Tok::Range(a, b) => {
                for cell in expand(&a, &b) {
                    out.insert(cell);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn expand(a: &CellRef, b: &CellRef) -> Vec<CellRef> {
    let mut out = Vec::new();
    for row in a.row.min(b.row)..=a.row.max(b.row) {
        for col in a.col.min(b.col)..=a.col.max(b.col) {
            out.push(CellRef {
                sheet: a.sheet.clone(),
                col,
                row,
            });
        }
    }
    out
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    values: &'a dyn Fn(&CellRef) -> Value,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn expr(&mut self) -> Value {
        let mut left = self.term();
        while let Some(tok) = self.peek().cloned() {
            match tok {
                Tok::Op(c @ ('+' | '-' | '&')) => {
                    self.pos += 1;
                    let right = self.term();
                    left = binary(c, left, right);
                }
                Tok::Cmp(op) => {
                    self.pos += 1;
                    let right = self.term();
                    left = compare(&op, left, right);
                }
                _ => break,
            }
        }
        left
    }

    fn term(&mut self) -> Value {
        let mut left = self.power();
        while let Some(Tok::Op(c @ ('*' | '/'))) = self.peek().cloned() {
            self.pos += 1;
            let right = self.power();
            left = binary(c, left, right);
        }
        left
    }

    fn power(&mut self) -> Value {
        let base = self.unary();
        if let Some(Tok::Op('^')) = self.peek() {
            self.pos += 1;
            let exp = self.power();
            return binary('^', base, exp);
        }
        base
    }

    fn unary(&mut self) -> Value {
        if let Some(Tok::Op('-')) = self.peek() {
            self.pos += 1;
            let v = self.unary();
            return match v.as_number() {
                Ok(n) => Value::Number(-n),
                Err(e) => e,
            };
        }
        self.atom()
    }

    fn collect_args(&mut self) -> Vec<Value> {
        let mut args = Vec::new();
        if let Some(Tok::LParen) = self.peek() {
            self.pos += 1;
        }
        loop {
            match self.peek() {
                None | Some(Tok::RParen) => {
                    self.pos += 1;
                    break;
                }
                Some(Tok::Comma) => {
                    self.pos += 1;
                }
                Some(Tok::Range(a, b)) => {
                    let (a, b) = (a.clone(), b.clone());
                    self.pos += 1;
                    for cell in expand(&a, &b) {
                        args.push((self.values)(&cell));
                    }
                }
                _ => args.push(self.expr()),
            }
        }
        args
    }

    fn atom(&mut self) -> Value {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Value::Number(n)
            }
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Value::Text(s)
            }
            Some(Tok::Ref(r)) => {
                self.pos += 1;
                (self.values)(&r)
            }
            Some(Tok::Range(a, b)) => {
                // A bare range outside a function is the first cell, as in most spreadsheets.
                self.pos += 1;
                expand(&a, &b)
                    .first()
                    .map(|c| (self.values)(c))
                    .unwrap_or(Value::Empty)
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let v = self.expr();
                if let Some(Tok::RParen) = self.peek() {
                    self.pos += 1;
                }
                v
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                let args = self.collect_args();
                call(&name, args)
            }
            _ => Value::Error("#VALUE!".into()),
        }
    }
}

fn binary(op: char, a: Value, b: Value) -> Value {
    if op == '&' {
        return Value::Text(format!("{}{}", a.render(), b.render()));
    }
    let (x, y) = match (a.as_number(), b.as_number()) {
        (Ok(x), Ok(y)) => (x, y),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    match op {
        '+' => Value::Number(x + y),
        '-' => Value::Number(x - y),
        '*' => Value::Number(x * y),
        '/' => {
            if y == 0.0 {
                Value::Error("#DIV/0!".into())
            } else {
                Value::Number(x / y)
            }
        }
        '^' => Value::Number(x.powf(y)),
        _ => Value::Error("#VALUE!".into()),
    }
}

fn compare(op: &str, a: Value, b: Value) -> Value {
    let ord = match (a.as_number(), b.as_number()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y),
        _ => a.render().partial_cmp(&b.render()),
    };
    let Some(ord) = ord else {
        return Value::Error("#VALUE!".into());
    };
    use std::cmp::Ordering::*;
    Value::Bool(match op {
        "=" => ord == Equal,
        "<" => ord == Less,
        ">" => ord == Greater,
        "<=" => ord != Greater,
        ">=" => ord != Less,
        "<>" => ord != Equal,
        _ => return Value::Error("#VALUE!".into()),
    })
}

fn call(name: &str, args: Vec<Value>) -> Value {
    if let Some(e) = args.iter().find(|a| a.is_error()) {
        return e.clone();
    }
    let nums: Vec<f64> = args.iter().filter_map(|a| a.as_number().ok()).collect();
    match name {
        "SUM" => Value::Number(nums.iter().sum()),
        "AVERAGE" | "AVG" => {
            if nums.is_empty() {
                Value::Error("#DIV/0!".into())
            } else {
                Value::Number(nums.iter().sum::<f64>() / nums.len() as f64)
            }
        }
        "MIN" => nums
            .iter()
            .copied()
            .fold(None::<f64>, |acc, n| Some(acc.map_or(n, |a| a.min(n))))
            .map(Value::Number)
            .unwrap_or(Value::Empty),
        "MAX" => nums
            .iter()
            .copied()
            .fold(None::<f64>, |acc, n| Some(acc.map_or(n, |a| a.max(n))))
            .map(Value::Number)
            .unwrap_or(Value::Empty),
        "COUNT" => Value::Number(nums.len() as f64),
        "ABS" => nums
            .first()
            .map(|n| Value::Number(n.abs()))
            .unwrap_or(Value::Empty),
        "ROUND" => {
            let n = nums.first().copied().unwrap_or(0.0);
            let places = nums.get(1).copied().unwrap_or(0.0) as i32;
            let f = 10f64.powi(places);
            Value::Number((n * f).round() / f)
        }
        "IF" => {
            let cond = match args.first() {
                Some(Value::Bool(b)) => *b,
                Some(v) => v.as_number().map(|n| n != 0.0).unwrap_or(false),
                None => false,
            };
            let idx = if cond { 1 } else { 2 };
            args.get(idx).cloned().unwrap_or(Value::Empty)
        }
        other => Value::Error(format!("#NAME? {other}")),
    }
}

/// Evaluate a workbook, or just the part of it that a change can reach.
///
/// `dirty` is the set of cells whose content changed. Everything that transitively depends on them
/// recalculates; everything else keeps the value it had. Passing an empty `dirty` set means a full
/// recalculation, which is what rung 3 does once at the end.
pub struct Engine<'a> {
    pub grid: &'a Grid,
    pub names: &'a HashMap<String, String>,
}

impl Engine<'_> {
    /// Cells that read `cell`, directly.
    pub fn dependents(&self) -> HashMap<CellRef, Vec<CellRef>> {
        let mut map: HashMap<CellRef, Vec<CellRef>> = HashMap::new();
        for (at, cell) in self.grid {
            if let Cell::Formula(f) = cell
                && let Ok(deps) = dependencies(&at.sheet, f, self.names)
            {
                for d in deps {
                    map.entry(d).or_default().push(at.clone());
                }
            }
        }
        map
    }

    /// The transitive closure of what a change touches. This is the recalc set (section 4.4).
    pub fn impacted(&self, dirty: &[CellRef]) -> BTreeSet<CellRef> {
        let dependents = self.dependents();
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<CellRef> = dirty.iter().cloned().collect();
        while let Some(c) = queue.pop_front() {
            if !seen.insert(c.clone()) {
                continue;
            }
            if let Some(ups) = dependents.get(&c) {
                queue.extend(ups.iter().cloned());
            }
        }
        seen
    }

    /// Evaluate, recalculating only what `dirty` can reach when it is non-empty.
    pub fn evaluate(
        &self,
        dirty: &[CellRef],
        previous: &BTreeMap<CellRef, Value>,
    ) -> BTreeMap<CellRef, Value> {
        let recalc: BTreeSet<CellRef> = if dirty.is_empty() {
            self.grid.keys().cloned().collect()
        } else {
            self.impacted(dirty)
        };

        let mut values: BTreeMap<CellRef, Value> = previous.clone();
        for (at, cell) in self.grid {
            if !recalc.contains(at) && values.contains_key(at) {
                continue;
            }
            if let Cell::Literal(v) = cell {
                values.insert(at.clone(), v.clone());
            }
        }

        // Evaluate formulas with memoisation and cycle detection.
        let mut in_progress: BTreeSet<CellRef> = BTreeSet::new();
        let order: Vec<CellRef> = self
            .grid
            .iter()
            .filter(|(at, c)| {
                matches!(c, Cell::Formula(_)) && (recalc.contains(*at) || !values.contains_key(*at))
            })
            .map(|(at, _)| at.clone())
            .collect();

        for at in order {
            let v = self.eval_cell(&at, &mut values, &mut in_progress);
            values.insert(at, v);
        }
        values
    }

    fn eval_cell(
        &self,
        at: &CellRef,
        values: &mut BTreeMap<CellRef, Value>,
        in_progress: &mut BTreeSet<CellRef>,
    ) -> Value {
        match self.grid.get(at) {
            None => Value::Empty,
            Some(Cell::Literal(v)) => v.clone(),
            Some(Cell::Formula(f)) => {
                if !in_progress.insert(at.clone()) {
                    return Value::Error("#CYCLE!".into());
                }
                let body = f.trim_start_matches('=').to_string();
                let toks = match tokenize(&at.sheet, &body, self.names) {
                    Ok(t) => t,
                    Err(e) => {
                        in_progress.remove(at);
                        return Value::Error(format!("#VALUE! {e}"));
                    }
                };

                // Resolve dependencies first, depth first, so evaluation order is irrelevant.
                let deps = dependencies(&at.sheet, &body, self.names).unwrap_or_default();
                for d in deps {
                    if !values.contains_key(&d) {
                        let v = self.eval_cell(&d, values, in_progress);
                        values.insert(d, v);
                    }
                }

                let snapshot = values.clone();
                let lookup = move |r: &CellRef| snapshot.get(r).cloned().unwrap_or(Value::Empty);
                let mut parser = Parser {
                    toks,
                    pos: 0,
                    values: &lookup,
                };
                let v = parser.expr();
                in_progress.remove(at);
                v
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(a1: &str) -> CellRef {
        CellRef::parse("Sheet1", a1).unwrap()
    }

    fn grid(pairs: &[(&str, Cell)]) -> Grid {
        pairs.iter().map(|(k, v)| (r(k), v.clone())).collect()
    }

    fn engine<'a>(g: &'a Grid, names: &'a HashMap<String, String>) -> Engine<'a> {
        Engine { grid: g, names }
    }

    #[test]
    fn cell_refs_round_trip_through_a1() {
        assert_eq!(r("A1").a1(), "Sheet1!A1");
        assert_eq!(r("AA10").a1(), "Sheet1!AA10");
        assert_eq!(r("$B$2").a1(), "Sheet1!B2");
        assert_eq!(
            CellRef::parse("Sheet1", "Other!C3").unwrap().a1(),
            "Other!C3"
        );
    }

    #[test]
    fn arithmetic_and_functions_evaluate() {
        let g = grid(&[
            ("A1", Cell::Literal(Value::Number(2.0))),
            ("A2", Cell::Literal(Value::Number(3.0))),
            ("A3", Cell::Formula("=A1+A2*2".into())),
            ("A4", Cell::Formula("=SUM(A1:A2)".into())),
            ("A5", Cell::Formula("=IF(A1>1,\"big\",\"small\")".into())),
        ]);
        let names = HashMap::new();
        let v = engine(&g, &names).evaluate(&[], &BTreeMap::new());
        assert_eq!(v[&r("A3")], Value::Number(8.0));
        assert_eq!(v[&r("A4")], Value::Number(5.0));
        assert_eq!(v[&r("A5")], Value::Text("big".into()));
    }

    #[test]
    fn division_by_zero_is_an_error_cell_not_a_panic() {
        let g = grid(&[
            ("A1", Cell::Literal(Value::Number(1.0))),
            ("A2", Cell::Literal(Value::Number(0.0))),
            ("A3", Cell::Formula("=A1/A2".into())),
        ]);
        let names = HashMap::new();
        let v = engine(&g, &names).evaluate(&[], &BTreeMap::new());
        assert_eq!(v[&r("A3")], Value::Error("#DIV/0!".into()));
    }

    #[test]
    fn a_cycle_is_reported_rather_than_hanging() {
        let g = grid(&[
            ("A1", Cell::Formula("=A2+1".into())),
            ("A2", Cell::Formula("=A1+1".into())),
        ]);
        let names = HashMap::new();
        let v = engine(&g, &names).evaluate(&[], &BTreeMap::new());
        assert!(v[&r("A1")].is_error() || v[&r("A2")].is_error(), "{v:?}");
    }

    #[test]
    fn dependencies_include_every_cell_of_a_range() {
        let names = HashMap::new();
        let deps = dependencies("Sheet1", "=SUM(A1:A3)+B1", &names).unwrap();
        assert_eq!(deps.len(), 4, "{deps:?}");
        assert!(deps.contains(&r("A2")));
    }

    #[test]
    fn impact_is_the_dependent_subgraph_and_nothing_else() {
        let g = grid(&[
            ("A1", Cell::Literal(Value::Number(1.0))),
            ("A2", Cell::Formula("=A1*2".into())),
            ("A3", Cell::Formula("=A2+1".into())),
            ("C1", Cell::Literal(Value::Number(99.0))),
            ("C2", Cell::Formula("=C1+1".into())),
        ]);
        let names = HashMap::new();
        let impacted = engine(&g, &names).impacted(&[r("A1")]);
        assert!(
            impacted.contains(&r("A3")),
            "transitive dependents are included"
        );
        assert!(
            !impacted.contains(&r("C2")),
            "an unrelated column must not recalculate: {impacted:?}"
        );
    }

    #[test]
    fn a_named_range_resolves() {
        let mut names = HashMap::new();
        names.insert("revenue".to_string(), "A1:A2".to_string());
        let g = grid(&[
            ("A1", Cell::Literal(Value::Number(10.0))),
            ("A2", Cell::Literal(Value::Number(5.0))),
            ("B1", Cell::Formula("=SUM(revenue)".into())),
        ]);
        let v = engine(&g, &names).evaluate(&[], &BTreeMap::new());
        assert_eq!(v[&r("B1")], Value::Number(15.0));
    }

    #[test]
    fn an_incremental_recalc_reaches_the_same_answer_as_a_full_one() {
        // The property that makes partial recalc safe to ship.
        let mut g = grid(&[
            ("A1", Cell::Literal(Value::Number(1.0))),
            ("A2", Cell::Formula("=A1*10".into())),
            ("A3", Cell::Formula("=A2+A1".into())),
            ("B1", Cell::Literal(Value::Number(7.0))),
        ]);
        let names = HashMap::new();
        let full_before = engine(&g, &names).evaluate(&[], &BTreeMap::new());

        g.insert(r("A1"), Cell::Literal(Value::Number(2.0)));
        let incremental = engine(&g, &names).evaluate(&[r("A1")], &full_before);
        let full_after = engine(&g, &names).evaluate(&[], &BTreeMap::new());

        assert_eq!(incremental[&r("A3")], full_after[&r("A3")]);
        assert_eq!(incremental[&r("A2")], Value::Number(20.0));
        assert_eq!(incremental[&r("B1")], Value::Number(7.0));
    }
}
