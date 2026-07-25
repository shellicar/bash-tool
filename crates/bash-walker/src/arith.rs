//! Shell arithmetic (`$((...))`, `((...))`, `for ((...))`, `[[ -eq ]]`
//! operands): 64-bit signed integers, bash's operator set and precedence,
//! short-circuit `&&`/`||`/`?:` (the skipped side is parsed but its side
//! effects — assignments, `++` — are suppressed, same as bash), and
//! recursive variable resolution (`x="1+2"; $((x))` is 3).

use crate::state::ShellState;

#[derive(Debug, thiserror::Error)]
#[error("arithmetic: {0}")]
pub struct ArithError(pub String);

pub fn eval(expr: &str, state: &mut ShellState) -> Result<i64, ArithError> {
    eval_depth(expr, state, 0)
}

fn eval_depth(expr: &str, state: &mut ShellState, depth: u32) -> Result<i64, ArithError> {
    if depth > 16 {
        return Err(ArithError("expression recursion level exceeded".into()));
    }
    let toks = tokenize(expr)?;
    let mut p = Parser { toks, pos: 0, state, depth };
    if p.toks.is_empty() {
        return Ok(0); // $(( )) with nothing is 0 in bash
    }
    let v = p.comma(true)?;
    if p.pos != p.toks.len() {
        return Err(ArithError(format!("unexpected token {:?}", p.toks[p.pos])));
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(i64),
    Ident(String),
    Op(&'static str),
}

const OPS: &[&str] = &[
    // longest first so the scanner matches greedily
    "<<=", ">>=", "**", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||",
    "+=", "-=", "*=", "/=", "%=", "&=", "^=", "|=",
    "+", "-", "*", "/", "%", "<", ">", "=", "!", "~", "&", "^", "|", "?", ":", "(", ")", ",",
];

fn tokenize(expr: &str) -> Result<Vec<Tok>, ArithError> {
    let b = expr.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    'outer: while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'#') {
                i += 1;
            }
            toks.push(Tok::Num(parse_number(&expr[start..i])?));
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            toks.push(Tok::Ident(expr[start..i].to_string()));
            continue;
        }
        for op in OPS {
            if expr[i..].starts_with(op) {
                toks.push(Tok::Op(op));
                i += op.len();
                continue 'outer;
            }
        }
        return Err(ArithError(format!("unexpected character {:?}", c as char)));
    }
    Ok(toks)
}

fn parse_number(s: &str) -> Result<i64, ArithError> {
    let bad = || ArithError(format!("invalid number {s:?}"));
    if let Some((base, digits)) = s.split_once('#') {
        let base: u32 = base.parse().map_err(|_| bad())?;
        if !(2..=36).contains(&base) {
            return Err(ArithError(format!("unsupported base {base} (2-36)")));
        }
        return i64::from_str_radix(&digits.to_lowercase(), base).map_err(|_| bad());
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).map_err(|_| bad());
    }
    if s.len() > 1 && s.starts_with('0') {
        return i64::from_str_radix(&s[1..], 8).map_err(|_| bad());
    }
    s.parse().map_err(|_| bad())
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    state: &'a mut ShellState,
    depth: u32,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn at_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(o)) if *o == op)
    }

    fn bump_op(&mut self, op: &str) -> bool {
        if self.at_op(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// A variable's value re-evaluated as an expression, bash-style:
    /// unset/empty is 0, "1+2" is 3.
    fn resolve(&mut self, name: &str, active: bool) -> Result<i64, ArithError> {
        if !active {
            return Ok(0);
        }
        match self.state.get_var(name) {
            None => Ok(0),
            Some(v) if v.trim().is_empty() => Ok(0),
            Some(v) => eval_depth(&v, self.state, self.depth + 1),
        }
    }

    fn assign(&mut self, name: &str, value: i64, active: bool) -> i64 {
        if active {
            self.state.set_var(name, value.to_string());
        }
        value
    }

    fn comma(&mut self, active: bool) -> Result<i64, ArithError> {
        let mut v = self.assignment(active)?;
        while self.bump_op(",") {
            v = self.assignment(active)?;
        }
        Ok(v)
    }

    fn assignment(&mut self, active: bool) -> Result<i64, ArithError> {
        // Lookahead: IDENT followed by an assignment operator.
        if let Some(Tok::Ident(name)) = self.peek().cloned() {
            if let Some(Tok::Op(op)) = self.toks.get(self.pos + 1) {
                let compound = matches!(
                    *op,
                    "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "^=" | "|=" | "<<=" | ">>="
                );
                if compound {
                    let op = *op;
                    self.pos += 2;
                    let rhs = self.assignment(active)?;
                    let v = if op == "=" {
                        rhs
                    } else {
                        let cur = self.resolve(&name, active)?;
                        apply_binop(&op[..op.len() - 1], cur, rhs, active)?
                    };
                    return Ok(self.assign(&name, v, active));
                }
            }
        }
        self.ternary(active)
    }

    fn ternary(&mut self, active: bool) -> Result<i64, ArithError> {
        let cond = self.binary(0, active)?;
        if !self.bump_op("?") {
            return Ok(cond);
        }
        let then_active = active && cond != 0;
        let v1 = self.assignment(then_active)?;
        if !self.bump_op(":") {
            return Err(ArithError("expected ':' in ?:".into()));
        }
        let else_active = active && cond == 0;
        let v2 = self.assignment(else_active)?;
        Ok(if cond != 0 { v1 } else { v2 })
    }

    /// Precedence climbing over the plain binary operators.
    fn binary(&mut self, min_prec: u8, active: bool) -> Result<i64, ArithError> {
        let mut lhs = self.unary(active)?;
        loop {
            let (op, prec) = match self.peek() {
                Some(Tok::Op(o)) => match binop_prec(o) {
                    Some(p) if p >= min_prec => (*o, p),
                    _ => break,
                },
                _ => break,
            };
            self.pos += 1;
            // Short-circuit: the dead side still parses, side effects off.
            let rhs_active = match op {
                "&&" => active && lhs != 0,
                "||" => active && lhs == 0,
                _ => active,
            };
            // `**` is right-associative; everything else left.
            let next_min = if op == "**" { prec } else { prec + 1 };
            let rhs = self.binary(next_min, rhs_active)?;
            lhs = match op {
                "&&" => i64::from(lhs != 0 && rhs != 0),
                "||" => i64::from(lhs != 0 || rhs != 0),
                _ => apply_binop(op, lhs, rhs, active)?,
            };
        }
        Ok(lhs)
    }

    fn unary(&mut self, active: bool) -> Result<i64, ArithError> {
        if self.bump_op("!") {
            return Ok(i64::from(self.unary(active)? == 0));
        }
        if self.bump_op("~") {
            return Ok(!self.unary(active)?);
        }
        if self.bump_op("-") {
            return Ok(self.unary(active)?.wrapping_neg());
        }
        if self.bump_op("+") {
            return self.unary(active);
        }
        if self.at_op("++") || self.at_op("--") {
            let delta = if self.at_op("++") { 1 } else { -1 };
            self.pos += 1;
            match self.peek().cloned() {
                Some(Tok::Ident(name)) => {
                    self.pos += 1;
                    let v = self.resolve(&name, active)?.wrapping_add(delta);
                    return Ok(self.assign(&name, v, active));
                }
                _ => return Err(ArithError("++/-- needs a variable".into())),
            }
        }
        self.postfix(active)
    }

    fn postfix(&mut self, active: bool) -> Result<i64, ArithError> {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Ok(n)
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                if self.at_op("++") || self.at_op("--") {
                    let delta = if self.at_op("++") { 1 } else { -1 };
                    self.pos += 1;
                    let v = self.resolve(&name, active)?;
                    self.assign(&name, v.wrapping_add(delta), active);
                    return Ok(v);
                }
                self.resolve(&name, active)
            }
            Some(Tok::Op("(")) => {
                self.pos += 1;
                let v = self.comma(active)?;
                if !self.bump_op(")") {
                    return Err(ArithError("expected ')'".into()));
                }
                Ok(v)
            }
            other => Err(ArithError(format!("unexpected {other:?}"))),
        }
    }
}

fn binop_prec(op: &str) -> Option<u8> {
    Some(match op {
        "||" => 1,
        "&&" => 2,
        "|" => 3,
        "^" => 4,
        "&" => 5,
        "==" | "!=" => 6,
        "<" | "<=" | ">" | ">=" => 7,
        "<<" | ">>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        "**" => 11,
        _ => return None,
    })
}

fn apply_binop(op: &str, l: i64, r: i64, active: bool) -> Result<i64, ArithError> {
    Ok(match op {
        "+" => l.wrapping_add(r),
        "-" => l.wrapping_sub(r),
        "*" => l.wrapping_mul(r),
        "/" => {
            if r == 0 {
                if !active {
                    return Ok(0); // dead branch of ?:/&&/|| never divides
                }
                return Err(ArithError("division by 0".into()));
            }
            l.wrapping_div(r)
        }
        "%" => {
            if r == 0 {
                if !active {
                    return Ok(0);
                }
                return Err(ArithError("division by 0".into()));
            }
            l.wrapping_rem(r)
        }
        "**" => {
            if r < 0 {
                return Err(ArithError("exponent less than 0".into()));
            }
            let mut acc: i64 = 1;
            for _ in 0..r {
                acc = acc.wrapping_mul(l);
            }
            acc
        }
        "<<" => l.wrapping_shl(r as u32),
        ">>" => l.wrapping_shr(r as u32),
        "<" => i64::from(l < r),
        "<=" => i64::from(l <= r),
        ">" => i64::from(l > r),
        ">=" => i64::from(l >= r),
        "==" => i64::from(l == r),
        "!=" => i64::from(l != r),
        "&" => l & r,
        "^" => l ^ r,
        "|" => l | r,
        other => return Err(ArithError(format!("unhandled operator {other}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ShellState {
        ShellState::default()
    }

    #[test]
    fn evaluates_precedence() {
        let expected = 14;

        let actual = eval("2 + 3 * 4", &mut state()).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn resolves_a_variable_recursively() {
        let mut s = state();
        s.set_var("x", "1+2".to_string());
        let expected = 6;

        let actual = eval("x * 2", &mut s).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn increments_mutate_the_variable() {
        let mut s = state();
        s.set_var("i", "5".to_string());
        let expected_result = 5; // post-increment yields the old value
        let expected_var = "6";

        let actual = eval("i++", &mut s).unwrap();

        assert_eq!(actual, expected_result);
        assert_eq!(s.get_var("i").unwrap(), expected_var);
    }

    #[test]
    fn short_circuit_suppresses_dead_side_effects() {
        let mut s = state();
        s.set_var("i", "1".to_string());
        let expected = "1";

        eval("0 && (i=99)", &mut s).unwrap();

        assert_eq!(s.get_var("i").unwrap(), expected);
    }

    #[test]
    fn ternary_takes_the_right_branch() {
        let expected = 7;

        let actual = eval("0 ? 3 : 7", &mut state()).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn hex_and_octal_literals() {
        let expected = 255 + 8;

        let actual = eval("0xff + 010", &mut state()).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn assignment_returns_and_stores() {
        let mut s = state();
        let expected = 4;

        let actual = eval("x = 2 + 2", &mut s).unwrap();

        assert_eq!(actual, expected);
        assert_eq!(s.get_var("x").unwrap(), "4");
    }

    #[test]
    fn division_by_zero_is_an_error() {
        let actual = eval("1 / 0", &mut state());

        assert!(actual.is_err());
    }
}
