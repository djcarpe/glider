//! A small Cypher-flavoured query language.
//!
//! Deliberately a subset, chosen so that every feature here actually works
//! rather than approximately works:
//!
//!   MATCH (a:Person {name:"Ada"})-[r:KNOWS*1..3]->(b) WHERE b.age > 30
//!     RETURN b.name, count(r) ORDER BY b.name DESC LIMIT 10
//!   CREATE (a:Person {name:"Ada", age:36})
//!   MATCH (a:Person),(b:Person) WHERE a.name="Ada" AND b.name="Bob"
//!     CREATE (a)-[:KNOWS {since:2020}]->(b)
//!   MATCH (n:Person) WHERE n.age IS NULL SET n.age = 0
//!   MATCH (n) WHERE id(n) = 4 DETACH DELETE n
//!   CALL pagerank(iterations: 30, top: 10, write: "rank")
//!   INDEX ON :Person(name)
//!   STATS / SCHEMA / COMPACT / CLEAR / HELP

use std::collections::BTreeSet;
use std::rc::Rc;
use std::collections::HashMap;

use std::collections::BTreeMap;
use crate::algo::{self, Adjacency};
use crate::graph::{Dir, Error, Graph, Projection as AlgoView, Result, Tier};
use crate::ooc::StateVec;
use crate::telemetry;
use crate::value::{parse_json, write_json_string, Value};

// ------------------------------------------------------------------- tokens

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    Sym(String),
}

impl Tok {
    fn ident(&self) -> Option<&str> {
        match self {
            Tok::Ident(s) => Some(s),
            _ => None,
        }
    }
}

/// The tokens a parameter's value stands for: exactly what the literal
/// would lex to, so a parameter works wherever a literal does (pattern
/// properties included, so indexes still apply) and can never become syntax.
fn value_tokens(v: &Value, out: &mut Vec<Tok>) {
    match v {
        Value::Null => out.push(Tok::Ident("null".into())),
        Value::Bool(b) => out.push(Tok::Ident(if *b { "true" } else { "false" }.into())),
        Value::Int(i) => out.push(Tok::Int(*i)),
        Value::Float(f) => out.push(Tok::Float(*f)),
        Value::Text(s) => out.push(Tok::Str(s.clone())),
        Value::List(items) => {
            out.push(Tok::Sym("[".into()));
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(Tok::Sym(",".into()));
                }
                value_tokens(x, out);
            }
            out.push(Tok::Sym("]".into()));
        }
    }
}

fn lex_with(src: &str, params: &[(String, Value)]) -> Result<Vec<Tok>> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Comments: // to end of line
        if c == '/' && i + 1 < b.len() && b[i + 1] == '/' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(b[start..i].iter().collect()));
            continue;
        }
        if c.is_ascii_digit()
            || (c == '-'
                && i + 1 < b.len()
                && b[i + 1].is_ascii_digit()
                && matches!(out.last(), None | Some(Tok::Sym(_))))
        {
            let start = i;
            if b[i] == '-' {
                i += 1;
            }
            let mut float = false;
            while i < b.len()
                && (b[i].is_ascii_digit() || b[i] == '.' || b[i] == 'e' || b[i] == 'E')
            {
                if b[i] == '.' {
                    // `..` is a range operator, not a decimal point
                    if i + 1 < b.len() && b[i + 1] == '.' {
                        break;
                    }
                    float = true;
                }
                i += 1;
            }
            let text: String = b[start..i].iter().collect();
            if float {
                out.push(Tok::Float(
                    text.parse()
                        .map_err(|_| Error::Msg(format!("bad number {}", text)))?,
                ));
            } else {
                match text.parse::<i64>() {
                    Ok(v) => out.push(Tok::Int(v)),
                    Err(_) => out.push(Tok::Float(
                        text.parse()
                            .map_err(|_| Error::Msg(format!("bad number {}", text)))?,
                    )),
                }
            }
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            while i < b.len() && b[i] != quote {
                if b[i] == '\\' && i + 1 < b.len() {
                    i += 1;
                    s.push(match b[i] {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                } else {
                    s.push(b[i]);
                }
                i += 1;
            }
            if i >= b.len() {
                return Err(Error::Msg("unterminated string".into()));
            }
            i += 1;
            out.push(Tok::Str(s));
            continue;
        }
        // `$name`: a parameter, replaced by its value.
        if c == '$' {
            i += 1;
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            let name: String = b[start..i].iter().collect();
            if name.is_empty() {
                return Err(Error::Msg("expected a parameter name after $".into()));
            }
            let v = params
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v)
                .ok_or_else(|| Error::Msg(format!("missing parameter ${name}")))?;
            value_tokens(v, &mut out);
            continue;
        }
        // Backtick-quoted identifiers, for labels with spaces.
        if c == '`' {
            i += 1;
            let start = i;
            while i < b.len() && b[i] != '`' {
                i += 1;
            }
            out.push(Tok::Ident(b[start..i].iter().collect()));
            i += 1;
            continue;
        }
        // Multi-character symbols first.
        let two: String = b[i..(i + 2).min(b.len())].iter().collect();
        if ["->", "<-", "<>", "<=", ">=", "!=", ".."].contains(&two.as_str()) {
            out.push(Tok::Sym(two));
            i += 2;
            continue;
        }
        if "(){}[]:,.=<>*-+/|!".contains(c) {
            out.push(Tok::Sym(c.to_string()));
            i += 1;
            continue;
        }
        if c == ';' {
            i += 1;
            continue;
        }
        return Err(Error::Msg(format!("unexpected character '{}'", c)));
    }
    Ok(out)
}

// -------------------------------------------------------------------- parser

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(Error::Msg(format!("expected {}", kw.to_uppercase())))
        }
    }

    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(x)) if x == s)
    }

    fn eat_sym(&mut self, s: &str) -> bool {
        if self.is_sym(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, s: &str) -> Result<()> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(Error::Msg(format!("expected '{}'", s)))
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.peek() {
            Some(Tok::Ident(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            _ => Err(Error::Msg("expected an identifier".into())),
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }
}

// ------------------------------------------------------------------ patterns

#[derive(Clone, Debug)]
struct NodePat {
    var: Option<Rc<str>>,
    labels: Vec<String>,
    props: Vec<(String, Expr)>,
}

#[derive(Clone, Debug)]
struct RelPat {
    var: Option<Rc<str>>,
    types: Vec<String>,
    dir: Dir,
    props: Vec<(String, Expr)>,
    /// (min, max) hops for variable-length patterns
    hops: Option<(u32, u32)>,
}

#[derive(Clone, Debug)]
struct Chain {
    nodes: Vec<NodePat>,
    rels: Vec<RelPat>,
}

// --------------------------------------------------------------- expressions

#[derive(Clone, Debug)]
enum Expr {
    Lit(Value),
    Var(String),
    Prop(String, String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp(String, Box<Expr>, Box<Expr>),
    Arith(char, Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>),
    Func(String, Vec<Expr>),
    List(Vec<Expr>),
}

impl Expr {
    /// Default column name, matching what the user typed closely enough that
    /// `ORDER BY n.name` can find the column produced by `RETURN n.name`.
    fn alias(&self) -> String {
        match self {
            Expr::Var(v) => v.clone(),
            Expr::Prop(v, k) => format!("{}.{}", v, k),
            Expr::Func(name, args) => {
                let inner: Vec<String> = args.iter().map(|a| a.alias()).collect();
                format!("{}({})", name.to_lowercase(), inner.join(", "))
            }
            Expr::Lit(v) => v.to_string(),
            _ => "expr".to_string(),
        }
    }

    fn is_aggregate(&self) -> bool {
        match self {
            Expr::Func(name, _) => matches!(
                name.to_lowercase().as_str(),
                "count" | "sum" | "avg" | "min" | "max" | "collect"
            ),
            _ => false,
        }
    }
}

// --------------------------------------------------------------- statements

#[derive(Clone, Debug)]
enum ReturnKind {
    Items(Vec<(Expr, String)>),
    All,
}

#[derive(Clone, Debug)]
enum SetItem {
    Prop(String, String, Expr),
    Label(String, String),
}

#[derive(Clone, Debug)]
enum Tail {
    Return {
        kind: ReturnKind,
        distinct: bool,
        order: Vec<(Expr, bool)>,
        skip: usize,
        limit: Option<usize>,
    },
    Set(Vec<SetItem>),
    Remove(Vec<SetItem>),
    Delete(Vec<String>, bool),
    Create(Vec<Chain>),
    Count,
}

#[derive(Clone, Debug)]
enum Stmt {
    Explain(Box<Stmt>),
    Match {
        patterns: Vec<Chain>,
        filter: Option<Expr>,
        tail: Tail,
    },
    Create(Vec<Chain>),
    Call {
        name: String,
        args: Vec<(String, Value)>,
    },
    Index {
        label: String,
        key: String,
        drop: bool,
    },
    Stats,
    Schema,
    Compact,
    Clear,
    Begin,
    Commit,
    Rollback,
    Help,
}

impl Stmt {
    /// The statement kind, as `db.operation.name` in telemetry.
    fn op(&self) -> &'static str {
        match self {
            Stmt::Explain(_) => "EXPLAIN",
            Stmt::Match { .. } => "MATCH",
            Stmt::Create(_) => "CREATE",
            Stmt::Call { .. } => "CALL",
            Stmt::Index { .. } => "INDEX",
            Stmt::Stats => "STATS",
            Stmt::Schema => "SCHEMA",
            Stmt::Compact => "COMPACT",
            Stmt::Clear => "CLEAR",
            Stmt::Begin => "BEGIN",
            Stmt::Commit => "COMMIT",
            Stmt::Rollback => "ROLLBACK",
            Stmt::Help => "HELP",
        }
    }
}

fn parse(src: &str) -> Result<Stmt> {
    parse_with(src, &[])
}

fn parse_with(src: &str, params: &[(String, Value)]) -> Result<Stmt> {
    let toks = lex_with(src, params)?;
    if toks.is_empty() {
        return Err(Error::Msg("empty statement".into()));
    }
    let mut p = Parser { toks, pos: 0 };

    if p.eat_kw("EXPLAIN") {
        let rest: String = src
            .trim_start()
            .get("EXPLAIN".len()..)
            .unwrap_or("")
            .to_string();
        return Ok(Stmt::Explain(Box::new(parse(&rest)?)));
    }

    if p.eat_kw("MATCH") {
        let patterns = parse_pattern_list(&mut p)?;
        let filter = if p.eat_kw("WHERE") {
            Some(parse_expr(&mut p)?)
        } else {
            None
        };
        let tail = parse_tail(&mut p)?;
        return Ok(Stmt::Match {
            patterns,
            filter,
            tail,
        });
    }
    if p.eat_kw("CREATE") {
        if p.is_kw("INDEX") {
            p.pos += 1;
            let (label, key) = parse_index_target(&mut p)?;
            return Ok(Stmt::Index {
                label,
                key,
                drop: false,
            });
        }
        return Ok(Stmt::Create(parse_pattern_list(&mut p)?));
    }
    if p.eat_kw("CALL") {
        let name = p.ident()?;
        let mut args = Vec::new();
        if p.eat_sym("(") {
            while !p.eat_sym(")") {
                let key = p.ident()?;
                p.expect_sym(":")?;
                let v = parse_expr(&mut p)?;
                args.push((key.to_lowercase(), const_value(&v)?));
                if !p.eat_sym(",") && !p.is_sym(")") {
                    return Err(Error::Msg("expected ',' or ')' in CALL arguments".into()));
                }
            }
        }
        return Ok(Stmt::Call { name, args });
    }
    if p.eat_kw("INDEX") {
        let (label, key) = parse_index_target(&mut p)?;
        return Ok(Stmt::Index {
            label,
            key,
            drop: false,
        });
    }
    if p.eat_kw("DROP") {
        p.expect_kw("INDEX")?;
        let (label, key) = parse_index_target(&mut p)?;
        return Ok(Stmt::Index {
            label,
            key,
            drop: true,
        });
    }
    if p.eat_kw("STATS") {
        return Ok(Stmt::Stats);
    }
    if p.eat_kw("SCHEMA") {
        return Ok(Stmt::Schema);
    }
    if p.eat_kw("COMPACT") {
        return Ok(Stmt::Compact);
    }
    if p.eat_kw("CLEAR") {
        return Ok(Stmt::Clear);
    }
    if p.eat_kw("BEGIN") {
        return Ok(Stmt::Begin);
    }
    if p.eat_kw("COMMIT") {
        return Ok(Stmt::Commit);
    }
    if p.eat_kw("ROLLBACK") {
        return Ok(Stmt::Rollback);
    }
    if p.eat_kw("HELP") {
        return Ok(Stmt::Help);
    }
    Err(Error::Msg(format!(
        "unrecognised statement starting at '{}'",
        p.peek().and_then(|t| t.ident()).unwrap_or("?")
    )))
}

fn parse_index_target(p: &mut Parser) -> Result<(String, String)> {
    p.eat_kw("ON");
    p.expect_sym(":")?;
    let label = p.ident()?;
    p.expect_sym("(")?;
    let key = p.ident()?;
    p.expect_sym(")")?;
    Ok((label, key))
}

fn parse_tail(p: &mut Parser) -> Result<Tail> {
    if p.eat_kw("RETURN") {
        let distinct = p.eat_kw("DISTINCT");
        let kind = if p.eat_sym("*") {
            ReturnKind::All
        } else {
            let mut items = Vec::new();
            loop {
                let e = parse_expr(p)?;
                let alias = if p.eat_kw("AS") {
                    p.ident()?
                } else {
                    e.alias()
                };
                items.push((e, alias));
                if !p.eat_sym(",") {
                    break;
                }
            }
            ReturnKind::Items(items)
        };
        let mut order = Vec::new();
        if p.eat_kw("ORDER") {
            p.expect_kw("BY")?;
            loop {
                let e = parse_expr(p)?;
                let desc = if p.eat_kw("DESC") {
                    true
                } else {
                    p.eat_kw("ASC");
                    false
                };
                order.push((e, desc));
                if !p.eat_sym(",") {
                    break;
                }
            }
        }
        let mut skip = 0usize;
        let mut limit = None;
        loop {
            if p.eat_kw("SKIP") {
                skip = expect_usize(p)?;
            } else if p.eat_kw("LIMIT") {
                limit = Some(expect_usize(p)?);
            } else {
                break;
            }
        }
        return Ok(Tail::Return {
            kind,
            distinct,
            order,
            skip,
            limit,
        });
    }
    if p.eat_kw("SET") {
        return Ok(Tail::Set(parse_set_items(p)?));
    }
    if p.eat_kw("REMOVE") {
        return Ok(Tail::Remove(parse_set_items(p)?));
    }
    if p.eat_kw("DETACH") {
        p.expect_kw("DELETE")?;
        return Ok(Tail::Delete(parse_var_list(p)?, true));
    }
    if p.eat_kw("DELETE") {
        return Ok(Tail::Delete(parse_var_list(p)?, true));
    }
    if p.eat_kw("CREATE") {
        return Ok(Tail::Create(parse_pattern_list(p)?));
    }
    if p.at_end() {
        return Ok(Tail::Count);
    }
    Err(Error::Msg(
        "expected RETURN, SET, REMOVE, DELETE or CREATE".into(),
    ))
}

fn expect_usize(p: &mut Parser) -> Result<usize> {
    match p.peek() {
        Some(Tok::Int(v)) if *v >= 0 => {
            let v = *v as usize;
            p.pos += 1;
            Ok(v)
        }
        _ => Err(Error::Msg("expected a non-negative integer".into())),
    }
}

fn parse_var_list(p: &mut Parser) -> Result<Vec<String>> {
    let mut vars = vec![p.ident()?];
    while p.eat_sym(",") {
        vars.push(p.ident()?);
    }
    Ok(vars)
}

fn parse_set_items(p: &mut Parser) -> Result<Vec<SetItem>> {
    let mut items = Vec::new();
    loop {
        let var = p.ident()?;
        if p.eat_sym(":") {
            items.push(SetItem::Label(var, p.ident()?));
        } else {
            p.expect_sym(".")?;
            let key = p.ident()?;
            if p.eat_sym("=") {
                items.push(SetItem::Prop(var, key, parse_expr(p)?));
            } else {
                items.push(SetItem::Prop(var, key, Expr::Lit(Value::Null)));
            }
        }
        if !p.eat_sym(",") {
            break;
        }
    }
    Ok(items)
}

fn parse_pattern_list(p: &mut Parser) -> Result<Vec<Chain>> {
    let mut chains = vec![parse_chain(p)?];
    while p.eat_sym(",") {
        chains.push(parse_chain(p)?);
    }
    Ok(chains)
}

fn parse_chain(p: &mut Parser) -> Result<Chain> {
    let mut nodes = vec![parse_node_pat(p)?];
    let mut rels = Vec::new();
    loop {
        let backward = p.is_sym("<-");
        if !(backward || p.is_sym("-")) {
            break;
        }
        p.pos += 1;
        let mut rel = RelPat {
            var: None,
            types: Vec::new(),
            dir: Dir::Both,
            props: Vec::new(),
            hops: None,
        };
        if p.eat_sym("[") {
            if let Some(Tok::Ident(v)) = p.peek() {
                let v = v.clone();
                p.pos += 1;
                rel.var = Some(Rc::from(v));
            }
            while p.eat_sym(":") {
                rel.types.push(p.ident()?);
                if !p.is_sym("|") {
                    break;
                }
                p.eat_sym("|");
            }
            if p.eat_sym("*") {
                let mut min = 1u32;
                let mut max = 10u32;
                if let Some(Tok::Int(v)) = p.peek() {
                    min = *v as u32;
                    max = min;
                    p.pos += 1;
                }
                if p.eat_sym("..") {
                    if let Some(Tok::Int(v)) = p.peek() {
                        max = *v as u32;
                        p.pos += 1;
                    } else {
                        max = 10;
                    }
                }
                rel.hops = Some((min.max(1), max));
            }
            if p.is_sym("{") {
                rel.props = parse_prop_map(p)?;
            }
            p.expect_sym("]")?;
        }
        let forward = if p.eat_sym("->") {
            true
        } else {
            p.expect_sym("-")?;
            false
        };
        rel.dir = match (backward, forward) {
            (false, true) => Dir::Out,
            (true, false) => Dir::In,
            _ => Dir::Both,
        };
        if rel.hops.is_some() && rel.var.is_some() {
            return Err(Error::Msg(
                "variable-length patterns can't bind a relationship variable".into(),
            ));
        }
        rels.push(rel);
        nodes.push(parse_node_pat(p)?);
    }
    Ok(Chain { nodes, rels })
}

fn parse_node_pat(p: &mut Parser) -> Result<NodePat> {
    p.expect_sym("(")?;
    let mut pat = NodePat {
        var: None,
        labels: Vec::new(),
        props: Vec::new(),
    };
    if let Some(Tok::Ident(v)) = p.peek() {
        let v = v.clone();
        p.pos += 1;
        pat.var = Some(Rc::from(v));
    }
    while p.eat_sym(":") {
        pat.labels.push(p.ident()?);
    }
    if p.is_sym("{") {
        pat.props = parse_prop_map(p)?;
    }
    p.expect_sym(")")?;
    Ok(pat)
}

fn parse_prop_map(p: &mut Parser) -> Result<Vec<(String, Expr)>> {
    p.expect_sym("{")?;
    let mut props = Vec::new();
    if p.eat_sym("}") {
        return Ok(props);
    }
    loop {
        let key = match p.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                p.pos += 1;
                s
            }
            _ => p.ident()?,
        };
        p.expect_sym(":")?;
        props.push((key, parse_expr(p)?));
        if !p.eat_sym(",") {
            break;
        }
    }
    p.expect_sym("}")?;
    Ok(props)
}

// Precedence: OR < AND < NOT < comparison < additive < multiplicative < atom
fn parse_expr(p: &mut Parser) -> Result<Expr> {
    parse_or(p)
}

fn parse_or(p: &mut Parser) -> Result<Expr> {
    let mut left = parse_and(p)?;
    while p.eat_kw("OR") {
        let right = parse_and(p)?;
        left = Expr::Or(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_and(p: &mut Parser) -> Result<Expr> {
    let mut left = parse_not(p)?;
    while p.eat_kw("AND") {
        let right = parse_not(p)?;
        left = Expr::And(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn parse_not(p: &mut Parser) -> Result<Expr> {
    if p.eat_kw("NOT") || p.eat_sym("!") {
        return Ok(Expr::Not(Box::new(parse_not(p)?)));
    }
    parse_comparison(p)
}

fn parse_comparison(p: &mut Parser) -> Result<Expr> {
    let left = parse_additive(p)?;

    if p.eat_kw("IS") {
        let negated = p.eat_kw("NOT");
        p.expect_kw("NULL")?;
        return Ok(Expr::IsNull(Box::new(left), !negated));
    }
    if p.eat_kw("IN") {
        p.expect_sym("[")?;
        let mut items = Vec::new();
        if !p.eat_sym("]") {
            loop {
                items.push(parse_expr(p)?);
                if !p.eat_sym(",") {
                    break;
                }
            }
            p.expect_sym("]")?;
        }
        return Ok(Expr::In(Box::new(left), items));
    }
    if p.eat_kw("CONTAINS") {
        return Ok(Expr::Cmp(
            "contains".into(),
            Box::new(left),
            Box::new(parse_additive(p)?),
        ));
    }
    if p.is_kw("STARTS") {
        p.pos += 1;
        p.expect_kw("WITH")?;
        return Ok(Expr::Cmp(
            "starts".into(),
            Box::new(left),
            Box::new(parse_additive(p)?),
        ));
    }
    if p.is_kw("ENDS") {
        p.pos += 1;
        p.expect_kw("WITH")?;
        return Ok(Expr::Cmp(
            "ends".into(),
            Box::new(left),
            Box::new(parse_additive(p)?),
        ));
    }

    for op in ["<>", "!=", "<=", ">=", "=", "<", ">"] {
        if p.is_sym(op) {
            p.pos += 1;
            let right = parse_additive(p)?;
            return Ok(Expr::Cmp(op.to_string(), Box::new(left), Box::new(right)));
        }
    }
    Ok(left)
}

fn parse_additive(p: &mut Parser) -> Result<Expr> {
    let mut left = parse_multiplicative(p)?;
    loop {
        if p.is_sym("+") {
            p.pos += 1;
            left = Expr::Arith('+', Box::new(left), Box::new(parse_multiplicative(p)?));
        } else if p.is_sym("-") {
            p.pos += 1;
            left = Expr::Arith('-', Box::new(left), Box::new(parse_multiplicative(p)?));
        } else {
            return Ok(left);
        }
    }
}

fn parse_multiplicative(p: &mut Parser) -> Result<Expr> {
    let mut left = parse_atom(p)?;
    loop {
        if p.is_sym("*") {
            p.pos += 1;
            left = Expr::Arith('*', Box::new(left), Box::new(parse_atom(p)?));
        } else if p.is_sym("/") {
            p.pos += 1;
            left = Expr::Arith('/', Box::new(left), Box::new(parse_atom(p)?));
        } else {
            return Ok(left);
        }
    }
}

fn parse_atom(p: &mut Parser) -> Result<Expr> {
    match p.peek().cloned() {
        Some(Tok::Int(v)) => {
            p.pos += 1;
            Ok(Expr::Lit(Value::Int(v)))
        }
        Some(Tok::Float(v)) => {
            p.pos += 1;
            Ok(Expr::Lit(Value::Float(v)))
        }
        Some(Tok::Str(s)) => {
            p.pos += 1;
            Ok(Expr::Lit(Value::Text(s)))
        }
        Some(Tok::Sym(s)) if s == "(" => {
            p.pos += 1;
            let e = parse_expr(p)?;
            p.expect_sym(")")?;
            Ok(e)
        }
        Some(Tok::Sym(s)) if s == "[" => {
            p.pos += 1;
            let mut items = Vec::new();
            if !p.eat_sym("]") {
                loop {
                    items.push(parse_expr(p)?);
                    if !p.eat_sym(",") {
                        break;
                    }
                }
                p.expect_sym("]")?;
            }
            Ok(Expr::List(items))
        }
        Some(Tok::Sym(s)) if s == "-" => {
            p.pos += 1;
            Ok(Expr::Arith(
                '-',
                Box::new(Expr::Lit(Value::Int(0))),
                Box::new(parse_atom(p)?),
            ))
        }
        Some(Tok::Ident(name)) => {
            p.pos += 1;
            let lower = name.to_lowercase();
            if lower == "true" {
                return Ok(Expr::Lit(Value::Bool(true)));
            }
            if lower == "false" {
                return Ok(Expr::Lit(Value::Bool(false)));
            }
            if lower == "null" {
                return Ok(Expr::Lit(Value::Null));
            }
            if p.is_sym("(") {
                p.pos += 1;
                let mut args = Vec::new();
                if p.eat_sym("*") {
                    args.push(Expr::Lit(Value::Text("*".into())));
                    p.expect_sym(")")?;
                    return Ok(Expr::Func(lower, args));
                }
                if !p.eat_sym(")") {
                    loop {
                        args.push(parse_expr(p)?);
                        if !p.eat_sym(",") {
                            break;
                        }
                    }
                    p.expect_sym(")")?;
                }
                return Ok(Expr::Func(lower, args));
            }
            if p.eat_sym(".") {
                let key = p.ident()?;
                return Ok(Expr::Prop(name, key));
            }
            Ok(Expr::Var(name))
        }
        other => Err(Error::Msg(format!("unexpected token {:?}", other))),
    }
}

fn const_value(e: &Expr) -> Result<Value> {
    match e {
        Expr::Lit(v) => Ok(v.clone()),
        Expr::List(items) => {
            let mut out = Vec::new();
            for i in items {
                out.push(const_value(i)?);
            }
            Ok(Value::List(out))
        }
        Expr::Arith('-', a, b) => {
            // `-5` parses as `0 - 5`; keep it an Int so a negative literal in
            // CREATE or CALL does not silently become a float.
            let (x, y) = (const_value(a)?, const_value(b)?);
            if let (Value::Int(x), Value::Int(y)) = (&x, &y) {
                return Ok(Value::Int(x - y));
            }
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => Ok(Value::Float(x - y)),
                _ => Err(Error::Msg("expected a constant".into())),
            }
        }
        _ => Err(Error::Msg("expected a constant value".into())),
    }
}

// ------------------------------------------------------------------ bindings

#[derive(Clone, Copy, Debug, PartialEq)]
enum Bind {
    Node(u64),
    Edge(u64),
}

/// Variable bindings for one row. Names are shared with the pattern, so
/// copying a row copies pointers, not strings.
type Binds = Vec<(Rc<str>, Bind)>;

fn lookup(binds: &Binds, var: &str) -> Option<Bind> {
    binds.iter().find(|(k, _)| &**k == var).map(|(_, v)| *v)
}

// ------------------------------------------------------------------- results

pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub message: Option<String>,
    pub touched: usize,
}

impl QueryResult {
    pub fn message(msg: impl Into<String>) -> QueryResult {
        QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            message: Some(msg.into()),
            touched: 0,
        }
    }

    pub fn table(columns: Vec<String>, rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns,
            rows,
            message: None,
            touched: 0,
        }
    }

    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"columns\":[");
        for (i, c) in self.columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(c, &mut out);
        }
        out.push_str("],\"rows\":[");
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('[');
            for (j, v) in row.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                v.write_json(&mut out);
            }
            out.push(']');
        }
        out.push(']');
        if let Some(m) = &self.message {
            out.push_str(",\"message\":");
            write_json_string(m, &mut out);
        }
        out.push_str(&format!(",\"touched\":{}", self.touched));
        out.push('}');
        out
    }
}

/// Render a node as a JSON object. Values are flat, so entities become text.
pub fn node_value(g: &Graph, id: u64) -> Value {
    let mut s = String::from("{\"id\":");
    s.push_str(&id.to_string());
    s.push_str(",\"labels\":[");
    for (i, l) in g.node_labels(id).iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        write_json_string(l, &mut s);
    }
    s.push_str("],\"props\":{");
    for (i, (k, v)) in g.node_props(id).iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        write_json_string(k, &mut s);
        s.push(':');
        v.write_json(&mut s);
    }
    s.push_str("}}");
    Value::Text(s)
}

pub fn edge_value(g: &Graph, id: u64) -> Value {
    let Some(e) = g.edge(id) else {
        return Value::Null;
    };
    let mut s = String::from("{\"id\":");
    s.push_str(&id.to_string());
    s.push_str(",\"type\":");
    write_json_string(g.edge_type_name(id).unwrap_or(""), &mut s);
    s.push_str(&format!(",\"from\":{},\"to\":{}", e.from, e.to));
    s.push_str(",\"props\":{");
    for (i, (k, v)) in g.edge_props(id).iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        write_json_string(k, &mut s);
        s.push(':');
        v.write_json(&mut s);
    }
    s.push_str("}}");
    Value::Text(s)
}

// ------------------------------------------------------------------- execute

pub fn execute(g: &mut Graph, src: &str) -> Result<QueryResult> {
    execute_with(g, src, &[])
}

/// Run a statement with parameters: `$name` in `src` stands for the value
/// named `name`, anywhere a literal may appear.
///
/// ```
/// # use glider::{Graph, query, value::Value};
/// let mut g = Graph::memory();
/// let p = [("name".to_string(), Value::from("Ada"))];
/// query::execute_with(&mut g, "CREATE (:Person {name: $name})", &p).unwrap();
/// let r = query::execute_with(&mut g, "MATCH (p:Person {name: $name}) RETURN p.name", &p).unwrap();
/// assert_eq!(r.rows[0][0], Value::from("Ada"));
/// ```
pub fn execute_with(g: &mut Graph, src: &str, params: &[(String, Value)]) -> Result<QueryResult> {
    let watch = telemetry::Stopwatch::start();
    let before = g.pager_stats();
    let mut report = telemetry::OpReport { op: "INVALID", ..Default::default() };
    let result = match parse_with(src, params) {
        Ok(stmt) => {
            report.op = stmt.op();
            if let Stmt::Call { name, .. } = &stmt {
                report.procedure = Some(name.clone());
            }
            execute_stmt(g, stmt)
        }
        Err(e) => Err(e),
    };
    let after = g.pager_stats();
    report.page_reads = after.reads.saturating_sub(before.reads);
    report.page_writes = after.writes.saturating_sub(before.writes);
    report.page_hits = after.hits.saturating_sub(before.hits);
    report.page_misses = after.misses.saturating_sub(before.misses);
    report.duration_ns = watch.elapsed_ns();
    match &result {
        Ok(r) => {
            report.rows = r.rows.len() as u64;
            report.touched = r.touched as u64;
        }
        Err(e) => report.error = Some(e.to_string()),
    }
    let db = telemetry::wants_db_metrics().then(|| (g.telemetry_id(), g.telemetry_name(), g.telemetry()));
    telemetry::finish(report, src, &watch, db);
    result
}

fn execute_stmt(g: &mut Graph, stmt: Stmt) -> Result<QueryResult> {
    // STATS reports the damage itself; everything else must not return
    // results computed from an image that failed a checksum along the way.
    let reports_damage = matches!(stmt, Stmt::Stats);
    let result = if matches!(stmt, Stmt::Begin | Stmt::Commit | Stmt::Rollback | Stmt::Compact | Stmt::Clear) {
        run(g, stmt)
    } else {
        atomically(g, |g| run(g, stmt))
    };
    if !reports_damage {
        if let Some(e) = g.integrity_error() {
            return Err(Error::Msg(format!(
                "database image is damaged: {e}. Run `glider <db> verify`; \
                 restore from a replica if it confirms the damage"
            )));
        }
    }
    result
}

/// A statement is one transaction: all of it commits, or (on an error)
/// none of it. Inside BEGIN it joins the open transaction instead.
fn atomically<R>(g: &mut Graph, f: impl FnOnce(&mut Graph) -> Result<R>) -> Result<R> {
    let auto = g.autocommit;
    g.autocommit = false;
    let r = f(g);
    g.autocommit = auto;
    if !auto || g.uncommitted() == 0 {
        return r;
    }
    match r {
        Ok(v) => {
            g.commit()?;
            Ok(v)
        }
        Err(e) => {
            let _ = g.rollback();
            Err(e)
        }
    }
}

fn run(g: &mut Graph, stmt: Stmt) -> Result<QueryResult> {
    match stmt {
        Stmt::Explain(inner) => explain(g, &inner),
        Stmt::Match {
            patterns,
            filter,
            tail,
        } => exec_match(g, &patterns, filter.as_ref(), &tail),
        Stmt::Create(chains) => {
            let mut binds: Binds = Vec::new();
            let (n, e) = create_chains(g, &chains, &mut binds)?;
            let mut result = QueryResult::message(format!("created {} nodes, {} edges", n, e));
            let ids: Vec<Vec<Value>> = binds
                .iter()
                .filter_map(|(_, b)| match b {
                    Bind::Node(id) => Some(vec![Value::Int(*id as i64)]),
                    _ => None,
                })
                .collect();
            if !ids.is_empty() {
                result.columns = vec!["id".into()];
                result.rows = ids;
            }
            result.touched = n + e;
            Ok(result)
        }
        Stmt::Call { name, args } => call_algorithm(g, &name, &args),
        Stmt::Index { label, key, drop } => {
            if drop {
                g.drop_index(&label, &key)?;
                Ok(QueryResult::message(format!(
                    "dropped index :{}({})",
                    label, key
                )))
            } else {
                g.create_index(&label, &key)?;
                Ok(QueryResult::message(format!(
                    "created index :{}({})",
                    label, key
                )))
            }
        }
        Stmt::Stats => {
            let s = g.stats();
            let mut rows = vec![
                vec![Value::Text("nodes".into()), Value::Int(s.nodes as i64)],
                vec![Value::Text("edges".into()), Value::Int(s.edges as i64)],
                vec![
                    Value::Text("labels".into()),
                    Value::Int(s.labels.len() as i64),
                ],
                vec![
                    Value::Text("edge_types".into()),
                    Value::Int(s.edge_types.len() as i64),
                ],
                vec![
                    Value::Text("indexes".into()),
                    Value::Int(s.indexes.len() as i64),
                ],
                vec![
                    Value::Text("interned_strings".into()),
                    Value::Int(s.interned as i64),
                ],
                vec![
                    Value::Text("file_bytes".into()),
                    Value::Int(s.file_bytes as i64),
                ],
                vec![
                    Value::Text("page_size".into()),
                    Value::Int(s.page_size as i64),
                ],
                vec![
                    Value::Text("cached_pages".into()),
                    Value::Int(s.cache_pages as i64),
                ],
                vec![
                    Value::Text("log_bytes".into()),
                    Value::Int(s.log_bytes as i64),
                ],
            ];
            if let Some((used, max)) = s.memory {
                rows.push(vec![Value::Text("memory_bytes".into()), Value::Int(used as i64)]);
                rows.push(vec![
                    Value::Text("max_memory".into()),
                    Value::Int(max.min(i64::MAX as u64) as i64),
                ]);
            }
            if let Some(e) = s.integrity_error {
                rows.push(vec![Value::Text("integrity_error".into()), Value::Text(e)]);
            }
            Ok(QueryResult::table(
                vec!["metric".into(), "value".into()],
                rows,
            ))
        }
        Stmt::Schema => {
            let s = g.stats();
            let mut rows = Vec::new();
            for (l, c) in s.labels {
                rows.push(vec![
                    Value::Text("label".into()),
                    Value::Text(l),
                    Value::Int(c as i64),
                ]);
            }
            for (t, c) in s.edge_types {
                rows.push(vec![
                    Value::Text("edge_type".into()),
                    Value::Text(t),
                    Value::Int(c as i64),
                ]);
            }
            for (l, k, c) in s.indexes {
                rows.push(vec![
                    Value::Text("index".into()),
                    Value::Text(format!(":{}({})", l, k)),
                    Value::Int(c as i64),
                ]);
            }
            Ok(QueryResult::table(
                vec!["kind".into(), "name".into(), "count".into()],
                rows,
            ))
        }
        Stmt::Compact => {
            let before = g.compact()?;
            let after = g.file_len();
            Ok(QueryResult::message(format!(
                "compacted {} -> {} bytes",
                before, after
            )))
        }
        Stmt::Clear => {
            g.clear()?;
            Ok(QueryResult::message("graph cleared"))
        }
        Stmt::Begin => {
            g.autocommit = false;
            Ok(QueryResult::message("transaction open (autocommit off)"))
        }
        Stmt::Commit => {
            g.commit()?;
            g.autocommit = true;
            Ok(QueryResult::message("committed"))
        }
        Stmt::Rollback => {
            let n = g.uncommitted();
            g.rollback()?;
            g.autocommit = true;
            Ok(QueryResult::message(format!("rolled back {n} changes")))
        }
        Stmt::Help => Ok(QueryResult::message(HELP.trim())),
    }
}

pub const HELP: &str = r#"
glider query language

  MATCH (a:Person {name:"Ada"})-[r:KNOWS]->(b) WHERE b.age > 30
        RETURN b.name, count(r) AS n ORDER BY n DESC LIMIT 10
  MATCH (a)-[:KNOWS*1..3]->(b) RETURN DISTINCT b            variable-length hops
  CREATE (a:Person {name:"Ada", age:36})
  CREATE (a:Person {name:"Ada"})-[:KNOWS {since:2020}]->(b:Person {name:"Bob"})
  MATCH (a:Person),(b:Person) WHERE a.name="Ada" AND b.name="Bob"
        CREATE (a)-[:KNOWS]->(b)
  MATCH (n:Person) WHERE n.age IS NULL SET n.age = 0, n:Unknown
  MATCH (n) WHERE id(n) = 4 DETACH DELETE n
  INDEX ON :Person(name)        DROP INDEX ON :Person(name)
  STATS   SCHEMA   COMPACT   CLEAR   BEGIN   COMMIT   ROLLBACK

functions       id(n) labels(n) type(r) degree(n) indegree(n) outdegree(n)
                length(x) lower(x) upper(x) abs(x) toInt(x) toFloat(x) coalesce(..)
aggregates      count(*) count(x) sum(x) avg(x) min(x) max(x) collect(x)
operators       = <> < <= > >= AND OR NOT IN [..] IS NULL CONTAINS
                STARTS WITH  ENDS WITH  + - * /

algorithms (CALL name(arg: value, ...))
  pagerank(damping, iterations, tolerance, dir, type, weight, top, write)
  betweenness(dir, type, samples, top, write)      closeness(dir, type, weighted, top, write)
  degree(dir, type, top, write)                    triangles(type, top, write)
  clustering(type, top, write)                     kcore(type, top, write)
  components(type, top, write)                     scc(type, top, write)
  communities(type, iterations, top, write)        shortestpath(from, to, dir, type, weight)
  sssp(from, dir, type, weight, top)               bfs(from, depth, dir, type, top)
  dfs(from, depth, dir, type, top)                 neighbors(from, dir, type)
  toposort(type, top)                              cycle(type)
  mst(type, weight)                                subgraph(from, depth, dir, type)

common args:  dir: "out" | "in" | "both"   type: "KNOWS"   weight: "cost"
              top: 10 (rows returned)      write: "score" (store result on nodes)
"#;

/// Show the plan instead of running it: which pattern element anchors the
/// match, why it was chosen, and the order the rest is walked in. Without
/// this, the fast and slow spellings of a query look identical.
fn explain(g: &Graph, stmt: &Stmt) -> Result<QueryResult> {
    let Stmt::Match {
        patterns, filter, ..
    } = stmt
    else {
        return Ok(QueryResult::message(
            "EXPLAIN currently covers MATCH; other statements have no plan to show",
        ));
    };

    let mut pins = Vec::new();
    pinned_ids(filter.as_ref(), &mut pins);

    let mut rows: Vec<Vec<Value>> = Vec::new();
    let binds: Binds = Vec::new();
    for (pi, chain) in patterns.iter().enumerate() {
        let plan = plan_chain(g, chain, &binds, &pins);
        let name = |i: usize| -> String {
            let pat = &chain.nodes[i];
            let var: Rc<str> = pat.var.clone().unwrap_or_else(|| format!("_{i}").into());
            if pat.labels.is_empty() {
                format!("({var})")
            } else {
                format!("({var}:{})", pat.labels.join(":"))
            }
        };

        let on = match plan.rel_anchor {
            Some(i) => format!("{}-[:{}]-{}", name(i), chain.rels[i].types[0], name(i + 1)),
            None => name(plan.anchor),
        };
        rows.push(vec![
            Value::Int(pi as i64),
            Value::Text("anchor".into()),
            Value::Text(on),
            Value::Text(plan.reason.clone()),
            Value::Int(plan.estimate as i64),
        ]);

        for step in &plan.steps {
            let rel = &chain.rels[step.rel];
            let dir = if step.reversed {
                flip(rel.dir)
            } else {
                rel.dir
            };
            let arrow = match dir {
                Dir::Out => "->",
                Dir::In => "<-",
                _ => "--",
            };
            let types = if rel.types.is_empty() {
                String::new()
            } else {
                format!(":{}", rel.types.join("|"))
            };
            let hops = match rel.hops {
                Some((min, max)) => format!("*{min}..{max}"),
                None => String::new(),
            };
            let detail = format!(
                "{}{}{} {} {}",
                arrow,
                types,
                hops,
                name(step.to),
                if step.reversed { "(reversed)" } else { "" }
            );
            let est = match rel.hops {
                Some((_, max)) => Value::Text(format!("fan-out^{max}")),
                None => Value::Text("fan-out".into()),
            };
            rows.push(vec![
                Value::Int(pi as i64),
                Value::Text("expand".into()),
                Value::Text(name(step.from)),
                Value::Text(detail.trim_end().to_string()),
                est,
            ]);
        }
    }

    if filter.is_some() {
        let pinned = pins
            .iter()
            .map(|(v, id)| format!("{v}=#{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(vec![
            Value::Int(-1),
            Value::Text("filter".into()),
            Value::Text("WHERE".into()),
            Value::Text(if pins.is_empty() {
                "applied after matching".into()
            } else {
                format!("pushed into anchor: {pinned}")
            }),
            Value::Null,
        ]);
    }

    Ok(QueryResult {
        columns: vec![
            "pattern".into(),
            "op".into(),
            "on".into(),
            "detail".into(),
            "estimate".into(),
        ],
        rows,
        message: None,
        touched: 0,
    })
}

// ------------------------------------------------------------ match pipeline

fn exec_match(
    g: &mut Graph,
    patterns: &[Chain],
    filter: Option<&Expr>,
    tail: &Tail,
) -> Result<QueryResult> {
    // Reading tails consume matches as they are produced, so nothing larger
    // than their own output is ever held.
    match tail {
        Tail::Count => {
            let mut n = 0usize;
            match_all(g, patterns, 0, Vec::new(), filter, &mut |_| {
                n += 1;
                Ok(true)
            })?;
            return Ok(QueryResult::message(format!("{} matches", n)));
        }
        Tail::Return {
            kind,
            distinct,
            order,
            skip,
            limit,
        } => {
            if let Some(r) = count_from_counters(g, patterns, filter, tail) {
                return Ok(r);
            }
            let mut proj = Projection::new(kind, *distinct, order, *skip, *limit);
            let mut err = None;
            match_all(g, patterns, 0, Vec::new(), filter, &mut |b| match proj.push(g, b) {
                Ok(go) => Ok(go),
                Err(e) => {
                    err = Some(e);
                    Ok(false)
                }
            })?;
            if let Some(e) = err {
                return Err(e);
            }
            return proj.finish();
        }
        _ => {}
    }
    // Writing tails need the full match set before they change anything.
    // It is spooled (spilling to temp files past the working memory), then
    // replayed in match order.
    let budget = g.algorithm_memory();
    let tmp = g.temp_dir();
    crate::ooc::with_budget(budget, tmp, || exec_write(g, patterns, filter, tail))
}

/// `RETURN count(..)` over a pattern the store already counts: every node,
/// one label, one index bucket, or one edge type. Answered from counters
/// instead of enumerating the matches. `None` when the query needs the
/// general path.
fn count_from_counters(g: &Graph, patterns: &[Chain], filter: Option<&Expr>, tail: &Tail) -> Option<QueryResult> {
    let Tail::Return { kind: ReturnKind::Items(items), distinct: false, skip: 0, limit, .. } = tail else {
        return None;
    };
    if filter.is_some() || patterns.len() != 1 || items.len() != 1 || *limit == Some(0) {
        return None;
    }
    let (expr, alias) = &items[0];
    let Expr::Func(f, args) = expr else { return None };
    if !f.eq_ignore_ascii_case("count") || args.len() != 1 {
        return None;
    }
    let counted = match &args[0] {
        Expr::Lit(Value::Text(s)) if s == "*" => None,
        Expr::Var(v) => Some(v.as_str()),
        _ => return None,
    };
    let chain = &patterns[0];
    let plain = |n: &NodePat| n.labels.is_empty() && n.props.is_empty();
    let n = match (chain.nodes.as_slice(), chain.rels.as_slice()) {
        ([n], []) => {
            if counted.is_some_and(|c| n.var.as_deref() != Some(c)) {
                return None;
            }
            match (n.labels.as_slice(), n.props.as_slice()) {
                ([], []) => g.node_count(),
                ([l], []) => g.label_count(l),
                ([l], [(k, Expr::Lit(v))]) if g.has_index(l, k) => g.indexed_lookup(l, k, v)?.len(),
                _ => return None,
            }
        }
        ([a, b], [r]) if !plain(a) && plain(b) && r.types.len() == 1 && r.props.is_empty() && r.hops.is_none()
            && a.var.is_some() && a.var != b.var && a.var != r.var && counted.is_none_or(|c| Some(c) != a.var.as_deref()) =>
        {
            // count over one typed hop from anchored nodes: the adjacency
            // range count of each anchor, no rows built.
            let t = g.strings.lookup(&r.types[0]);
            let mut n = 0usize;
            let mut pins = Vec::new();
            pinned_ids(filter, &mut pins);
            let binds: Binds = Vec::new();
            let src = anchor_source(g, a, &binds, &pins);
            if !matches!(src, Source::Index) {
                return None;
            }
            if let Some(t) = t {
                for_each_candidate(g, a, &binds, &pins, false, &mut |id| {
                    if node_matches(g, a, id, &binds) {
                        n += g.degree_of(id, r.dir, Some(t));
                    }
                    Ok(true)
                })
                .ok()?;
            }
            n
        }
        ([a, b], [r]) => {
            // Distinct variables only: `(e)-[:R]->(e)` is a self-loop filter.
            let vars = [a.var.as_deref(), r.var.as_deref(), b.var.as_deref()];
            let named: Vec<_> = vars.iter().flatten().collect();
            if named.len() != named.iter().collect::<BTreeSet<_>>().len() {
                return None;
            }
            if counted.is_some_and(|c| !named.contains(&&c)) {
                return None;
            }
            if !plain(a) || !plain(b) || !r.props.is_empty() || r.hops.is_some() || r.dir == Dir::Both {
                return None;
            }
            match r.types.as_slice() {
                [] => g.edge_count(),
                [t] => g.type_count(t),
                _ => return None,
            }
        }
        _ => return None,
    };
    // As the general path: no matches, no rows.
    let rows = if n == 0 { vec![] } else { vec![vec![Value::Int(n as i64)]] };
    Some(QueryResult::table(vec![alias.clone()], rows))
}

/// Matched rows, spooled compactly: variable names once, then per row its
/// length and (name, node-or-edge id) pairs.
struct Spool {
    names: Vec<Rc<str>>,
    q: crate::ooc::SpillQueue<(u64, u64)>,
}

const EDGE_BIT: u64 = 1 << 63;

impl Spool {
    fn new() -> Spool {
        Spool {
            names: Vec::new(),
            q: crate::ooc::SpillQueue::new(),
        }
    }

    fn push(&mut self, b: &Binds) {
        self.q.push_back((u64::MAX, b.len() as u64));
        for (name, bind) in b {
            let k = match self.names.iter().position(|n| n == name) {
                Some(k) => k,
                None => {
                    self.names.push(name.clone());
                    self.names.len() - 1
                }
            };
            let v = match bind {
                Bind::Node(id) => *id,
                Bind::Edge(id) => *id | EDGE_BIT,
            };
            self.q.push_back((k as u64, v));
        }
    }

    fn next(&mut self) -> Option<Binds> {
        let (_, len) = self.q.pop_front()?;
        let mut b = Vec::with_capacity(len as usize);
        for _ in 0..len {
            let (k, v) = self.q.pop_front()?;
            let bind = if v & EDGE_BIT != 0 {
                Bind::Edge(v & !EDGE_BIT)
            } else {
                Bind::Node(v)
            };
            b.push((self.names[k as usize].clone(), bind));
        }
        Some(b)
    }
}

/// Ids in ascending order without duplicates, however many.
fn sorted_ids(sorter: crate::storage::extsort::Sorter, mut f: impl FnMut(u64) -> Result<()>) -> Result<()> {
    let io = |e: std::io::Error| Error::Msg(format!("sorting ids: {e}"));
    let mut it = sorter.finish().map_err(io)?;
    let mut last = None;
    while let Some((k, _)) = it.next().map_err(io)? {
        let id = u64::from_be_bytes(k[..8].try_into().unwrap());
        if last != Some(id) {
            f(id)?;
            last = Some(id);
        }
    }
    Ok(())
}

fn exec_write(g: &mut Graph, patterns: &[Chain], filter: Option<&Expr>, tail: &Tail) -> Result<QueryResult> {
    let mut spool = Spool::new();
    match_all(g, patterns, 0, Vec::new(), filter, &mut |b| {
        spool.push(&b);
        Ok(true)
    })?;
    let mut rows = std::iter::from_fn(move || spool.next());

    match tail {
        Tail::Count | Tail::Return { .. } => unreachable!("handled above"),
        Tail::Set(items) => {
            let mut touched = 0;
            for binds in rows.by_ref() {
                let binds = &binds;
                for item in items {
                    match item {
                        SetItem::Prop(var, key, expr) => {
                            let v = eval(g, expr, binds);
                            match lookup(binds, var) {
                                Some(Bind::Node(id)) => {
                                    g.set_node_prop(id, key, v)?;
                                    touched += 1;
                                }
                                Some(Bind::Edge(id)) => {
                                    g.set_edge_prop(id, key, v)?;
                                    touched += 1;
                                }
                                None => {
                                    return Err(Error::Msg(format!("unknown variable {}", var)))
                                }
                            }
                        }
                        SetItem::Label(var, label) => {
                            if let Some(Bind::Node(id)) = lookup(binds, var) {
                                g.add_label(id, label)?;
                                touched += 1;
                            }
                        }
                    }
                }
            }
            let mut r = QueryResult::message(format!("set {} values", touched));
            r.touched = touched;
            Ok(r)
        }
        Tail::Remove(items) => {
            let mut touched = 0;
            for binds in rows.by_ref() {
                let binds = &binds;
                for item in items {
                    match item {
                        SetItem::Prop(var, key, _) => match lookup(binds, var) {
                            Some(Bind::Node(id)) => {
                                g.unset_node_prop(id, key)?;
                                touched += 1;
                            }
                            Some(Bind::Edge(id)) => {
                                g.unset_edge_prop(id, key)?;
                                touched += 1;
                            }
                            None => return Err(Error::Msg(format!("unknown variable {}", var))),
                        },
                        SetItem::Label(var, label) => {
                            if let Some(Bind::Node(id)) = lookup(binds, var) {
                                g.remove_label(id, label)?;
                                touched += 1;
                            }
                        }
                    }
                }
            }
            let mut r = QueryResult::message(format!("removed {} values", touched));
            r.touched = touched;
            Ok(r)
        }
        Tail::Delete(vars, _detach) => {
            use crate::storage::extsort::Sorter;
            let io = |e: std::io::Error| Error::Msg(format!("sorting ids: {e}"));
            let dir = crate::ooc::temp_dir();
            let per = (crate::ooc::budget_left().unwrap_or(1 << 30) / 4).clamp(1 << 20, 256 << 20) as usize;
            let mut nodes = Sorter::new(&dir, "del-nodes", per).map_err(io)?;
            let mut edges = Sorter::new(&dir, "del-edges", per).map_err(io)?;
            for binds in rows.by_ref() {
                for var in vars {
                    match lookup(&binds, var) {
                        Some(Bind::Node(id)) => nodes.push(&id.to_be_bytes(), &[]).map_err(io)?,
                        Some(Bind::Edge(id)) => edges.push(&id.to_be_bytes(), &[]).map_err(io)?,
                        None => return Err(Error::Msg(format!("unknown variable {}", var))),
                    }
                }
            }
            let mut deleted = 0;
            sorted_ids(edges, |id| {
                if g.delete_edge(id)? {
                    deleted += 1;
                }
                Ok(())
            })?;
            sorted_ids(nodes, |id| {
                if g.delete_node(id)? {
                    deleted += 1;
                }
                Ok(())
            })?;
            let mut r = QueryResult::message(format!("deleted {} entities", deleted));
            r.touched = deleted;
            Ok(r)
        }
        Tail::Create(chains) => {
            let mut created_n = 0;
            let mut created_e = 0;
            for mut b in rows.by_ref() {
                let (n, e) = create_chains(g, chains, &mut b)?;
                created_n += n;
                created_e += e;
            }
            let mut r =
                QueryResult::message(format!("created {} nodes, {} edges", created_n, created_e));
            r.touched = created_n + created_e;
            Ok(r)
        }
    }
}
// ------------------------------------------------------------------ planning
//
// The matcher used to start at whichever node pattern was written first and
// walk left to right. That made `MATCH (a:Person)-[:KNOWS]->(b:Person
// {email:"x"})` scan every Person while the index on `email` sat unused,
// purely because `b` came second — and made
// `MATCH (a)-[*1..3]->(b) WHERE id(a) = 1` anchor on all 100,000 nodes.
//
// Now every node pattern is costed, the cheapest becomes the anchor, and the
// chain is walked outward from there — rightwards along the relationships as
// written, then leftwards with each direction flipped. `EXPLAIN` prints the
// result so the choice is visible rather than folklore.

/// One traversal: from an already-bound node pattern, across a relationship,
/// to a not-yet-bound one. `reversed` means we are walking the relationship
/// against the way it was written, so its direction flips.
#[derive(Clone, Copy, Debug)]
struct Step {
    from: usize,
    rel: usize,
    to: usize,
    reversed: bool,
}

struct Plan {
    anchor: usize,
    /// Start from the edges of one type instead: this relationship's two
    /// endpoints are bound together, then the walk goes out both ways.
    rel_anchor: Option<usize>,
    /// Estimated rows the anchor produces, and why.
    estimate: usize,
    reason: String,
    steps: Vec<Step>,
}

fn flip(d: Dir) -> Dir {
    match d {
        Dir::Out => Dir::In,
        Dir::In => Dir::Out,
        other => other,
    }
}

/// `WHERE id(x) = 7` is a pin, not a filter: it identifies exactly one node,
/// so it belongs in anchor selection rather than in a scan of everything.
/// Only conjunctions qualify — under an OR the predicate does not have to hold.
fn pinned_ids(filter: Option<&Expr>, out: &mut Vec<(String, u64)>) {
    let Some(expr) = filter else { return };
    match expr {
        Expr::And(a, b) => {
            pinned_ids(Some(a), out);
            pinned_ids(Some(b), out);
        }
        Expr::Cmp(op, a, b) if op == "=" => {
            for (x, y) in [(a, b), (b, a)] {
                if let (Expr::Func(name, args), Expr::Lit(Value::Int(n))) = (&**x, &**y) {
                    if name.eq_ignore_ascii_case("id") && args.len() == 1 && *n >= 0 {
                        if let Expr::Var(v) = &args[0] {
                            out.push((v.clone(), *n as u64));
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// What a node pattern costs to enumerate, and the reason to show in EXPLAIN.
fn estimate(g: &Graph, pat: &NodePat, binds: &Binds, pins: &[(String, u64)]) -> (usize, String) {
    if let Some(v) = &pat.var {
        if lookup(binds, v).is_some() {
            return (1, format!("bound variable {v}"));
        }
        if pins.iter().any(|(name, _)| **name == **v) {
            return (1, format!("id({v}) predicate"));
        }
    }
    for label in &pat.labels {
        for (key, expr) in &pat.props {
            if let Expr::Lit(v) = expr {
                if let Some(n) = g.index_count(label, key, v) {
                    return (n, format!("index :{label}({key})"));
                }
            }
        }
    }
    if let Some(label) = pat.labels.first() {
        return (g.label_count(label), format!("label scan :{label}"));
    }
    (g.node_count(), "all nodes".to_string())
}

/// Whether the planner may anchor on an edge type. Tests turn it off to check
/// that both plans give the same rows.
#[doc(hidden)]
pub static EDGE_ANCHORS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn plan_chain(g: &Graph, chain: &Chain, binds: &Binds, pins: &[(String, u64)]) -> Plan {
    let mut anchor = 0;
    let mut best = usize::MAX;
    let mut reason = String::new();
    for (i, pat) in chain.nodes.iter().enumerate() {
        let (est, why) = estimate(g, pat, binds, pins);
        if est < best {
            best = est;
            anchor = i;
            reason = why;
        }
    }

    // An edge type can be the cheaper place to start: `()-[r:RARE]->()`
    // should read the RARE edges, not every node's adjacency.
    let mut rel_anchor = None;
    let edges_ok = EDGE_ANCHORS.load(std::sync::atomic::Ordering::Relaxed);
    for (i, rel) in chain.rels.iter().enumerate() {
        if !edges_ok || rel.hops.is_some() || rel.types.len() != 1 {
            continue;
        }
        let est = g.type_count(&rel.types[0]);
        if est < best {
            best = est;
            rel_anchor = Some(i);
            reason = format!("edges :{}", rel.types[0]);
        }
    }

    // Outward from the anchor: right as written, then left with directions
    // flipped. Every step starts from a node bound by an earlier step. An
    // edge anchor binds nodes i and i+1, so the walk starts either side.
    let (right_from, left_from) = match rel_anchor {
        Some(i) => (i + 1, i),
        None => (anchor, anchor),
    };
    let mut steps = Vec::with_capacity(chain.rels.len());
    for i in right_from..chain.rels.len() {
        steps.push(Step {
            from: i,
            rel: i,
            to: i + 1,
            reversed: false,
        });
    }
    for i in (0..left_from).rev() {
        steps.push(Step {
            from: i + 1,
            rel: i,
            to: i,
            reversed: true,
        });
    }

    Plan {
        anchor: rel_anchor.unwrap_or(anchor),
        rel_anchor,
        estimate: best,
        reason,
        steps,
    }
}

/// Where matched rows go. Returning `false` stops the match early.
type Sink<'a> = dyn FnMut(Binds) -> Result<bool> + 'a;

/// Match the comma-separated chains left to right, each against every
/// binding the previous ones produced, then apply WHERE, streaming the
/// survivors into `sink`. Returns false if the sink stopped it.
fn match_all(
    g: &Graph,
    patterns: &[Chain],
    i: usize,
    binds: Binds,
    filter: Option<&Expr>,
    sink: &mut Sink<'_>,
) -> Result<bool> {
    if i == patterns.len() {
        if let Some(f) = filter {
            if !eval(g, f, &binds).truthy() {
                return Ok(true);
            }
        }
        return sink(binds);
    }
    match_chain_filtered(g, &patterns[i], binds, filter, &mut |b| {
        match_all(g, patterns, i + 1, b, filter, sink)
    })
}

fn match_chain_filtered(
    g: &Graph,
    chain: &Chain,
    binds: Binds,
    filter: Option<&Expr>,
    out: &mut Sink<'_>,
) -> Result<bool> {
    let mut pins = Vec::new();
    pinned_ids(filter, &mut pins);
    let plan = plan_chain(g, chain, &binds, &pins);
    if let Some(ri) = plan.rel_anchor {
        return match_from_edges(g, chain, &plan, ri, binds, out);
    }
    let pat = &chain.nodes[plan.anchor];
    // A label scan's candidates carry the label already: with nothing else
    // to check, they need no read. Otherwise read records by scanning.
    let from_label_scan = matches!(anchor_source(g, pat, &binds, &pins), Source::Label | Source::All);
    let already = from_label_scan && pat.labels.len() <= 1 && pat.props.is_empty();
    let prime = from_label_scan && (pat.var.is_some() || filter.is_some() || !already);

    let mut go = true;
    for_each_candidate(g, pat, &binds, &pins, prime, &mut |id| {
        if !already && !node_matches(g, pat, id, &binds) {
            return Ok(true);
        }
        let mut b = binds.clone();
        if let Some(v) = &pat.var {
            match lookup(&b, v) {
                Some(existing) if existing != Bind::Node(id) => return Ok(true),
                Some(_) => {}
                None => b.push((v.clone(), Bind::Node(id))),
            }
        }
        let mut bound = vec![None; chain.nodes.len()];
        bound[plan.anchor] = Some(id);
        go = walk(g, chain, &plan.steps, 0, b, bound, &[], out)?;
        Ok(go)
    })?;
    Ok(go)
}

/// Bind a pattern variable, or check it against its existing binding.
fn bind(b: &mut Binds, var: &Option<Rc<str>>, value: Bind) -> bool {
    let Some(v) = var else { return true };
    match lookup(b, v) {
        Some(existing) => existing == value,
        None => {
            b.push((v.clone(), value));
            true
        }
    }
}

/// Match a chain anchored on the edges of one type, streamed in pages of
/// edge ids.
fn match_from_edges(g: &Graph, chain: &Chain, plan: &Plan, ri: usize, binds: Binds, out: &mut Sink<'_>) -> Result<bool> {
    let rel = &chain.rels[ri];
    let Some(t) = g.strings.lookup(&rel.types[0]) else {
        return Ok(true);
    };
    let (lp, rp) = (&chain.nodes[ri], &chain.nodes[ri + 1]);
    let mut result = Ok(true);
    g.scan_type_edges(t, &mut |e| {
        if !edge_props_match(g, rel, e.id, &binds) {
            return true;
        }
        // As written, then (undirected) the other way round: the same rows
        // a walk from either end would produce.
        let ends: &[(u64, u64)] = match rel.dir {
            Dir::Out => &[(e.from, e.to)],
            Dir::In => &[(e.to, e.from)],
            Dir::Both => &[(e.from, e.to), (e.to, e.from)],
        };
        for &(a, b_) in ends {
            if !node_matches(g, lp, a, &binds) || !node_matches(g, rp, b_, &binds) {
                continue;
            }
            let mut b = binds.clone();
            if !bind(&mut b, &lp.var, Bind::Node(a))
                || !bind(&mut b, &rel.var, Bind::Edge(e.id))
                || !bind(&mut b, &rp.var, Bind::Node(b_))
            {
                continue;
            }
            let mut bound = vec![None; chain.nodes.len()];
            bound[ri] = Some(a);
            bound[ri + 1] = Some(b_);
            match walk(g, chain, &plan.steps, 0, b, bound, &[e.id], out) {
                Ok(true) => {}
                other => {
                    result = other;
                    return false;
                }
            }
        }
        true
    });
    result
}

/// Candidate ids for an anchor, cheapest source first, streamed in pages
/// of ids so a scan of a huge label or of every node never materialises the
/// whole list. Stops when `f` returns false.
#[derive(PartialEq)]
enum Source {
    Bound,
    Pin,
    Index,
    Label,
    All,
}

/// Where `for_each_candidate` will take an anchor's ids from.
fn anchor_source(g: &Graph, pat: &NodePat, binds: &Binds, pins: &[(String, u64)]) -> Source {
    if let Some(v) = &pat.var {
        if let Some(Bind::Node(_)) = lookup(binds, v) {
            return Source::Bound;
        }
        if pins.iter().any(|(name, _)| **name == **v) {
            return Source::Pin;
        }
    }
    for label in &pat.labels {
        for (key, expr) in &pat.props {
            if matches!(expr, Expr::Lit(_)) && g.has_index(label, key) {
                return Source::Index;
            }
        }
    }
    if pat.labels.is_empty() {
        Source::All
    } else {
        Source::Label
    }
}

fn for_each_candidate(
    g: &Graph,
    pat: &NodePat,
    binds: &Binds,
    pins: &[(String, u64)],
    prime: bool,
    f: &mut dyn FnMut(u64) -> Result<bool>,
) -> Result<bool> {
    if let Some(v) = &pat.var {
        if let Some(Bind::Node(id)) = lookup(binds, v) {
            return f(id);
        }
        if let Some((_, id)) = pins.iter().find(|(name, _)| **name == **v) {
            return if g.node(*id).is_some() { f(*id) } else { Ok(true) };
        }
    }
    for label in &pat.labels {
        for (key, expr) in &pat.props {
            if let Expr::Lit(v) = expr {
                if g.has_index(label, key) {
                    if let Some(ids) = g.indexed_lookup(label, key, v) {
                        for id in ids {
                            if !f(id)? {
                                return Ok(false);
                            }
                        }
                        return Ok(true);
                    }
                }
            }
        }
    }
    const PAGE: usize = 4096;
    let label = match pat.labels.first() {
        Some(l) => match g.strings.lookup(l) {
            Some(id) => Some(id),
            None => return Ok(true),
        },
        None => None,
    };
    if prime {
        // Read records in id order with one cursor, each left in the read
        // cache for the pattern check, WHERE and RETURN that follow.
        let mut err = None;
        let r = g.scan_nodes(label, 0, usize::MAX, &mut |id| match f(id) {
            Ok(go) => go,
            Err(e) => {
                err = Some(e);
                false
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        return Ok(r.is_some());
    }
    let mut from = 0u64;
    loop {
        let ids = match label {
            Some(l) => g.label_members_from(l, from, PAGE),
            None => g.nodes_from(from, PAGE),
        };
        let n = ids.len();
        for id in ids {
            if !f(id)? {
                return Ok(false);
            }
            from = id + 1;
        }
        if n < PAGE {
            return Ok(true);
        }
    }
}

/// Execute the plan, one step at a time, backtracking on failure.
/// `used` holds the relationships this path has already traversed: as in
/// Cypher, a match may not use the same relationship twice.
#[allow(clippy::too_many_arguments)]
fn walk(
    g: &Graph,
    chain: &Chain,
    steps: &[Step],
    i: usize,
    binds: Binds,
    bound: Vec<Option<u64>>,
    used: &[u64],
    out: &mut Sink<'_>,
) -> Result<bool> {
    if i >= steps.len() {
        return out(binds);
    }
    let step = steps[i];
    let rel = &chain.rels[step.rel];
    let target_pat = &chain.nodes[step.to];
    let dir = if step.reversed {
        flip(rel.dir)
    } else {
        rel.dir
    };
    let current = match bound[step.from] {
        Some(id) => id,
        None => return Ok(true),
    };

    let type_ids: Vec<u32> = rel
        .types
        .iter()
        .filter_map(|t| g.strings.lookup(t))
        .collect();
    if !rel.types.is_empty() && type_ids.len() != rel.types.len() {
        return Ok(true);
    }

    if let Some((min, max)) = rel.hops {
        let mut frontier = vec![current];
        let mut seen = crate::graph::id_set();
        seen.insert(current);
        for depth in 1..=max {
            let mut next = Vec::new();
            let only = if type_ids.len() == 1 { Some(type_ids[0]) } else { None };
            for node in &frontier {
                for adj in g.neighbors(*node, dir, only) {
                    if !type_ids.is_empty() && !type_ids.contains(&adj.etype) {
                        continue;
                    }
                    if !rel.props.is_empty() && !edge_props_match(g, rel, adj.edge, &binds) {
                        continue;
                    }
                    if !seen.insert(adj.other) {
                        continue;
                    }
                    next.push(adj.other);
                }
            }
            if depth >= min {
                for id in &next {
                    if !node_matches(g, target_pat, *id, &binds) {
                        continue;
                    }
                    let mut b = binds.clone();
                    if let Some(v) = &target_pat.var {
                        match lookup(&b, v) {
                            Some(existing) if existing != Bind::Node(*id) => continue,
                            Some(_) => {}
                            None => b.push((v.clone(), Bind::Node(*id))),
                        }
                    }
                    let mut bound2 = bound.clone();
                    bound2[step.to] = Some(*id);
                    if !walk(g, chain, steps, i + 1, b, bound2, used, out)? {
                        return Ok(false);
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        return Ok(true);
    }

    // One type: a range scan of just that type's adjacency.
    let only = if type_ids.len() == 1 { Some(type_ids[0]) } else { None };
    for adj in g.neighbors(current, dir, only) {
        if !type_ids.is_empty() && !type_ids.contains(&adj.etype) {
            continue;
        }
        if used.contains(&adj.edge) {
            continue;
        }
        if !edge_props_match(g, rel, adj.edge, &binds) {
            continue;
        }
        if !node_matches(g, target_pat, adj.other, &binds) {
            continue;
        }
        let mut b = binds.clone();
        if let Some(v) = &rel.var {
            match lookup(&b, v) {
                Some(existing) if existing != Bind::Edge(adj.edge) => continue,
                Some(_) => {}
                None => b.push((v.clone(), Bind::Edge(adj.edge))),
            }
        }
        if let Some(v) = &target_pat.var {
            match lookup(&b, v) {
                Some(existing) if existing != Bind::Node(adj.other) => continue,
                Some(_) => {}
                None => b.push((v.clone(), Bind::Node(adj.other))),
            }
        }
        let mut bound2 = bound.clone();
        bound2[step.to] = Some(adj.other);
        let mut used2 = Vec::with_capacity(used.len() + 1);
        used2.extend_from_slice(used);
        used2.push(adj.edge);
        if !walk(g, chain, steps, i + 1, b, bound2, &used2, out)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether node `id` satisfies a pattern's labels and properties. Ids come
/// from adjacency, label, index or id scans, all of which only hold live
/// nodes, so a pattern with neither needs no read at all; otherwise the
/// record is read once for both checks.
fn node_matches(g: &Graph, pat: &NodePat, id: u64, binds: &Binds) -> bool {
    if pat.labels.is_empty() && pat.props.is_empty() {
        return true;
    }
    let Some(n) = g.node(id) else {
        return false;
    };
    for label in &pat.labels {
        match g.strings.lookup(label) {
            Some(l) if n.has_label(l) => {}
            _ => return false,
        }
    }
    for (key, expr) in &pat.props {
        let want = eval(g, expr, binds);
        match g.strings.lookup(key).and_then(|k| n.prop(k)) {
            Some(got) if got == want => {}
            _ => return false,
        }
    }
    true
}

fn edge_props_match(g: &Graph, rel: &RelPat, edge: u64, binds: &Binds) -> bool {
    for (key, expr) in &rel.props {
        let want = eval(g, expr, binds);
        match g.edge_prop(edge, key) {
            Some(got) if got == want => {}
            _ => return false,
        }
    }
    true
}

// ------------------------------------------------------------------- create

fn create_chains(g: &mut Graph, chains: &[Chain], binds: &mut Binds) -> Result<(usize, usize)> {
    let mut nodes = 0;
    let mut edges = 0;
    for chain in chains {
        let mut ids: Vec<u64> = Vec::with_capacity(chain.nodes.len());
        for pat in &chain.nodes {
            // A bare `(a)` referring to an already-bound variable reuses it.
            if pat.labels.is_empty() && pat.props.is_empty() {
                if let Some(v) = &pat.var {
                    if let Some(Bind::Node(id)) = lookup(binds, v) {
                        ids.push(id);
                        continue;
                    }
                }
            }
            let props: Vec<(String, Value)> = pat
                .props
                .iter()
                .map(|(k, e)| (k.clone(), eval(g, e, binds)))
                .collect();
            let id = g.add_node(&pat.labels, props)?;
            nodes += 1;
            if let Some(v) = &pat.var {
                binds.push((v.clone(), Bind::Node(id)));
            }
            ids.push(id);
        }
        for (i, rel) in chain.rels.iter().enumerate() {
            if rel.hops.is_some() {
                return Err(Error::Msg(
                    "variable-length patterns can't be created".into(),
                ));
            }
            let etype = rel.types.first().cloned().ok_or_else(|| {
                Error::Msg("CREATE needs a relationship type, e.g. -[:KNOWS]->".into())
            })?;
            let props: Vec<(String, Value)> = rel
                .props
                .iter()
                .map(|(k, e)| (k.clone(), eval(g, e, binds)))
                .collect();
            let (from, to) = match rel.dir {
                Dir::In => (ids[i + 1], ids[i]),
                _ => (ids[i], ids[i + 1]),
            };
            let eid = g.add_edge(from, to, &etype, props)?;
            edges += 1;
            if let Some(v) = &rel.var {
                binds.push((v.clone(), Bind::Edge(eid)));
            }
        }
    }
    Ok((nodes, edges))
}

// --------------------------------------------------------------- projection

/// A row or group key, ordered by `total_cmp` with -0.0 folded into 0.0:
/// every pair of values that `==` calls equal lands on the same key, so a
/// lookup narrows to a few candidates that are then checked with `==`.
#[derive(Clone)]
struct EqKey(Vec<Value>);

fn fold_zero(v: &Value) -> Value {
    match v {
        Value::Float(f) if *f == 0.0 => Value::Float(0.0),
        Value::List(items) => Value::List(items.iter().map(fold_zero).collect()),
        other => other.clone(),
    }
}

impl EqKey {
    fn of(row: &[Value]) -> EqKey {
        EqKey(row.iter().map(fold_zero).collect())
    }
}

impl PartialEq for EqKey {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == std::cmp::Ordering::Equal
    }
}
impl Eq for EqKey {}
impl PartialOrd for EqKey {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for EqKey {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        for (a, b) in self.0.iter().zip(o.0.iter()) {
            let c = a.total_cmp(b);
            if c != std::cmp::Ordering::Equal {
                return c;
            }
        }
        self.0.len().cmp(&o.0.len())
    }
}

/// Rows, deduplicated by `==` keeping the first of each, in arrival order.
#[derive(Default)]
struct FirstSeen {
    buckets: BTreeMap<EqKey, Vec<usize>>,
}

impl FirstSeen {
    /// The index of an earlier row equal to `row`, or record `row` as `idx`.
    fn find_or_add(&mut self, rows: &[Vec<Value>], row: &[Value], idx: usize) -> Option<usize> {
        let bucket = self.buckets.entry(EqKey::of(row)).or_default();
        for &i in bucket.iter() {
            if rows[i].as_slice() == row {
                return Some(i);
            }
        }
        bucket.push(idx);
        None
    }
}

/// One aggregate's running state.
enum Acc {
    Count(i64),
    Sum(f64),
    Avg(f64, u64),
    Min(Option<Value>),
    Max(Option<Value>),
    Collect(Vec<Value>),
    Other,
}

impl Acc {
    fn new(e: &Expr) -> Acc {
        let Expr::Func(name, _) = e else {
            return Acc::Other;
        };
        match name.as_str() {
            "count" => Acc::Count(0),
            "sum" => Acc::Sum(0.0),
            "avg" => Acc::Avg(0.0, 0),
            "min" => Acc::Min(None),
            "max" => Acc::Max(None),
            "collect" => Acc::Collect(Vec::new()),
            _ => Acc::Other,
        }
    }

    fn add(&mut self, g: &Graph, e: &Expr, binds: &Binds) {
        let Expr::Func(_, args) = e else { return };
        let arg = args.first();
        let star = matches!(arg, Some(Expr::Lit(Value::Text(s))) if s == "*");
        if let Acc::Count(n) = self {
            // count only asks whether the value is null: never render it (a
            // bound variable is never null).
            let counts = match arg {
                _ if star => true,
                Some(Expr::Var(v)) => lookup(binds, v).is_some(),
                Some(a) => !matches!(eval(g, a, binds), Value::Null),
                None => false,
            };
            *n += counts as i64;
            return;
        }
        let v = if star {
            Value::Int(1)
        } else {
            match arg {
                Some(a) => eval(g, a, binds),
                None => return,
            }
        };
        if matches!(v, Value::Null) {
            return;
        }
        match self {
            Acc::Sum(s) => {
                if let Some(x) = v.as_f64() {
                    *s += x;
                }
            }
            Acc::Avg(s, n) => {
                if let Some(x) = v.as_f64() {
                    *s += x;
                    *n += 1;
                }
            }
            // First of equal minimums, last of equal maximums, as min_by and
            // max_by pick them.
            Acc::Min(m) => {
                if m.as_ref().map(|c| v.total_cmp(c) == std::cmp::Ordering::Less).unwrap_or(true) {
                    *m = Some(v);
                }
            }
            Acc::Max(m) => {
                if m.as_ref().map(|c| v.total_cmp(c) != std::cmp::Ordering::Less).unwrap_or(true) {
                    *m = Some(v);
                }
            }
            Acc::Collect(l) => l.push(v),
            Acc::Count(_) | Acc::Other => {}
        }
    }

    fn value(self) -> Value {
        match self {
            Acc::Count(n) => Value::Int(n),
            Acc::Sum(s) => Value::Float(s),
            Acc::Avg(s, n) => {
                if n == 0 {
                    Value::Null
                } else {
                    Value::Float(s / n as f64)
                }
            }
            Acc::Min(m) | Acc::Max(m) => m.unwrap_or(Value::Null),
            Acc::Collect(l) => Value::List(l),
            Acc::Other => Value::Null,
        }
    }
}

/// RETURN, fed one match at a time. Holds only what the output needs: the
/// groups of an aggregate, the top rows of an ORDER BY ... LIMIT, the rows
/// seen so far of a LIMIT (and stops the match once it has enough).
struct Projection<'q> {
    kind: &'q ReturnKind,
    distinct: bool,
    order: &'q [(Expr, bool)],
    skip: usize,
    limit: Option<usize>,
    items: Option<Vec<(Expr, String)>>,
    keys: Vec<(usize, bool)>,
    has_agg: bool,
    rows: Vec<Vec<Value>>,
    seen: FirstSeen,
    groups: Vec<(Vec<Value>, Vec<Acc>)>,
    group_index: FirstSeen,
    group_keys: Vec<Vec<Value>>,
    /// Rows produced before SKIP/LIMIT (after DISTINCT).
    total: usize,
}

impl<'q> Projection<'q> {
    fn new(
        kind: &'q ReturnKind,
        distinct: bool,
        order: &'q [(Expr, bool)],
        skip: usize,
        limit: Option<usize>,
    ) -> Projection<'q> {
        Projection {
            kind,
            distinct,
            order,
            skip,
            limit,
            items: None,
            keys: Vec::new(),
            has_agg: false,
            rows: Vec::new(),
            seen: FirstSeen::default(),
            groups: Vec::new(),
            group_index: FirstSeen::default(),
            group_keys: Vec::new(),
            total: 0,
        }
    }

    /// Resolve the items (RETURN * takes the first row's variables) and the
    /// ORDER BY columns.
    fn resolve(&mut self, first: Option<&Binds>) -> Result<()> {
        if self.items.is_some() {
            return Ok(());
        }
        let items: Vec<(Expr, String)> = match self.kind {
            ReturnKind::Items(items) => items.clone(),
            ReturnKind::All => first
                .map(|b| b.iter().map(|(k, _)| (Expr::Var(k.to_string()), k.to_string())).collect())
                .unwrap_or_default(),
        };
        let columns: Vec<String> = items.iter().map(|(_, a)| a.clone()).collect();
        self.keys = self
            .order
            .iter()
            .map(|(e, desc)| {
                let alias = e.alias();
                let idx = columns.iter().position(|c| *c == alias).ok_or_else(|| {
                    Error::Msg(format!("ORDER BY {} is not a returned column", alias))
                })?;
                Ok((idx, *desc))
            })
            .collect::<Result<Vec<_>>>()?;
        self.has_agg = items.iter().any(|(e, _)| e.is_aggregate());
        self.items = Some(items);
        Ok(())
    }

    fn cmp_rows(keys: &[(usize, bool)], a: &[Value], b: &[Value]) -> std::cmp::Ordering {
        for (idx, desc) in keys {
            let ord = a[*idx].total_cmp(&b[*idx]);
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    }

    /// Take one match. Returns false once no more are needed.
    fn push(&mut self, g: &Graph, binds: Binds) -> Result<bool> {
        self.resolve(Some(&binds))?;
        let items = self.items.as_ref().expect("resolved");
        if self.has_agg {
            let key: Vec<Value> = items
                .iter()
                .filter(|(e, _)| !e.is_aggregate())
                .map(|(e, _)| eval(g, e, &binds))
                .collect();
            let idx = self.groups.len();
            let gi = match self.group_index.find_or_add(&self.group_keys, &key, idx) {
                Some(i) => i,
                None => {
                    let accs = items
                        .iter()
                        .filter(|(e, _)| e.is_aggregate())
                        .map(|(e, _)| Acc::new(e))
                        .collect();
                    self.group_keys.push(key.clone());
                    self.groups.push((key, accs));
                    idx
                }
            };
            let aggs = items.iter().filter(|(e, _)| e.is_aggregate()).map(|(e, _)| e);
            for (acc, e) in self.groups[gi].1.iter_mut().zip(aggs) {
                acc.add(g, e, &binds);
            }
            return Ok(true);
        }
        let row: Vec<Value> = items.iter().map(|(e, _)| eval(g, e, &binds)).collect();
        if self.distinct {
            let idx = self.rows.len();
            if self.seen.find_or_add(&self.rows, &row, idx).is_some() {
                return Ok(true);
            }
        }
        self.total += 1;
        self.rows.push(row);
        let Some(limit) = self.limit else {
            return Ok(true);
        };
        let want = self.skip.saturating_add(limit);
        if self.keys.is_empty() {
            // No order: the first rows are the answer.
            return Ok(self.rows.len() < want);
        }
        if !self.distinct && self.rows.len() >= want.saturating_mul(2).max(want + 1024) {
            // Keep only the best `want`; a stable sort keeps ties in arrival
            // order, as sorting everything at the end would.
            let keys = self.keys.clone();
            self.rows.sort_by(|a, b| Self::cmp_rows(&keys, a, b));
            self.rows.truncate(want);
        }
        Ok(true)
    }

    fn finish(mut self) -> Result<QueryResult> {
        self.resolve(None)?;
        let items = self.items.take().expect("resolved");
        let columns: Vec<String> = items.iter().map(|(_, a)| a.clone()).collect();
        let mut out_rows: Vec<Vec<Value>> = if self.has_agg {
            let rows: Vec<Vec<Value>> = std::mem::take(&mut self.groups)
                .into_iter()
                .map(|(key, accs)| {
                    let mut key = key.into_iter();
                    let mut accs = accs.into_iter();
                    items
                        .iter()
                        .map(|(e, _)| {
                            if e.is_aggregate() {
                                accs.next().map(Acc::value).unwrap_or(Value::Null)
                            } else {
                                key.next().unwrap_or(Value::Null)
                            }
                        })
                        .collect()
                })
                .collect();
            let rows = if self.distinct {
                let mut seen = FirstSeen::default();
                let mut kept: Vec<Vec<Value>> = Vec::new();
                for r in rows {
                    let idx = kept.len();
                    if seen.find_or_add(&kept, &r, idx).is_none() {
                        kept.push(r);
                    }
                }
                kept
            } else {
                rows
            };
            self.total = rows.len();
            rows
        } else {
            std::mem::take(&mut self.rows)
        };
        if !self.keys.is_empty() {
            let keys = self.keys.clone();
            out_rows.sort_by(|a, b| Self::cmp_rows(&keys, a, b));
        }
        let mut final_rows: Vec<Vec<Value>> = out_rows.into_iter().skip(self.skip).collect();
        if let Some(l) = self.limit {
            final_rows.truncate(l);
        }
        Ok(QueryResult {
            columns,
            rows: final_rows,
            message: None,
            touched: self.total,
        })
    }
}

// -------------------------------------------------------------- expressions

fn eval(g: &Graph, e: &Expr, binds: &Binds) -> Value {
    match e {
        Expr::Lit(v) => v.clone(),
        Expr::List(items) => Value::List(items.iter().map(|i| eval(g, i, binds)).collect()),
        Expr::Var(v) => match lookup(binds, v) {
            Some(Bind::Node(id)) => node_value(g, id),
            Some(Bind::Edge(id)) => edge_value(g, id),
            None => Value::Null,
        },
        Expr::Prop(var, key) => match lookup(binds, var) {
            Some(Bind::Node(id)) => g.node_prop(id, key).unwrap_or(Value::Null),
            Some(Bind::Edge(id)) => g.edge_prop(id, key).unwrap_or(Value::Null),
            None => Value::Null,
        },
        Expr::Not(inner) => Value::Bool(!eval(g, inner, binds).truthy()),
        Expr::And(a, b) => Value::Bool(eval(g, a, binds).truthy() && eval(g, b, binds).truthy()),
        Expr::Or(a, b) => Value::Bool(eval(g, a, binds).truthy() || eval(g, b, binds).truthy()),
        Expr::IsNull(inner, want_null) => {
            let is_null = matches!(eval(g, inner, binds), Value::Null);
            Value::Bool(is_null == *want_null)
        }
        Expr::In(inner, items) => {
            let v = eval(g, inner, binds);
            Value::Bool(items.iter().any(|i| eval(g, i, binds) == v))
        }
        Expr::Cmp(op, a, b) => {
            let (x, y) = (eval(g, a, binds), eval(g, b, binds));
            let result = match op.as_str() {
                "=" => x == y,
                "<>" | "!=" => x != y,
                "<" => x.total_cmp(&y) == std::cmp::Ordering::Less,
                "<=" => x.total_cmp(&y) != std::cmp::Ordering::Greater,
                ">" => x.total_cmp(&y) == std::cmp::Ordering::Greater,
                ">=" => x.total_cmp(&y) != std::cmp::Ordering::Less,
                "contains" => x.to_string().contains(&y.to_string()),
                "starts" => x.to_string().starts_with(&y.to_string()),
                "ends" => x.to_string().ends_with(&y.to_string()),
                _ => false,
            };
            Value::Bool(result)
        }
        Expr::Arith(op, a, b) => {
            let (x, y) = (eval(g, a, binds), eval(g, b, binds));
            if *op == '+' {
                if let (Value::Text(_), _) | (_, Value::Text(_)) = (&x, &y) {
                    return Value::Text(format!("{}{}", x, y));
                }
            }
            let (Some(xf), Some(yf)) = (x.as_f64(), y.as_f64()) else {
                return Value::Null;
            };
            let both_int = matches!(x, Value::Int(_)) && matches!(y, Value::Int(_));
            let out = match op {
                '+' => xf + yf,
                '-' => xf - yf,
                '*' => xf * yf,
                '/' => {
                    if yf == 0.0 {
                        return Value::Null;
                    }
                    xf / yf
                }
                _ => return Value::Null,
            };
            if both_int && *op != '/' {
                Value::Int(out as i64)
            } else {
                Value::Float(out)
            }
        }
        Expr::Func(name, args) => eval_func(g, name, args, binds),
    }
}

fn eval_func(g: &Graph, name: &str, args: &[Expr], binds: &Binds) -> Value {
    let arg_bind = |i: usize| -> Option<Bind> {
        match args.get(i) {
            Some(Expr::Var(v)) => lookup(binds, v),
            _ => None,
        }
    };
    let val = |i: usize| -> Value {
        args.get(i)
            .map(|a| eval(g, a, binds))
            .unwrap_or(Value::Null)
    };

    match name {
        "id" => match arg_bind(0) {
            Some(Bind::Node(id)) | Some(Bind::Edge(id)) => Value::Int(id as i64),
            None => Value::Null,
        },
        "labels" => match arg_bind(0) {
            Some(Bind::Node(id)) => {
                Value::List(g.node_labels(id).into_iter().map(Value::Text).collect())
            }
            _ => Value::Null,
        },
        "type" => match arg_bind(0) {
            Some(Bind::Edge(id)) => g
                .edge_type_name(id)
                .map(|t| Value::Text(t.to_string()))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "degree" | "outdegree" | "indegree" => match arg_bind(0) {
            Some(Bind::Node(id)) => {
                let dir = match name {
                    "outdegree" => Dir::Out,
                    "indegree" => Dir::In,
                    _ => Dir::Both,
                };
                Value::Int(g.degree(id, dir) as i64)
            }
            _ => Value::Null,
        },
        "keys" => match arg_bind(0) {
            Some(Bind::Node(id)) => Value::List(
                g.node_props(id)
                    .into_iter()
                    .map(|(k, _)| Value::Text(k))
                    .collect(),
            ),
            Some(Bind::Edge(id)) => Value::List(
                g.edge_props(id)
                    .into_iter()
                    .map(|(k, _)| Value::Text(k))
                    .collect(),
            ),
            None => Value::Null,
        },
        "length" | "size" => match val(0) {
            Value::Text(t) => Value::Int(t.chars().count() as i64),
            Value::List(l) => Value::Int(l.len() as i64),
            _ => Value::Null,
        },
        "lower" | "tolower" => Value::Text(val(0).to_string().to_lowercase()),
        "upper" | "toupper" => Value::Text(val(0).to_string().to_uppercase()),
        "trim" => Value::Text(val(0).to_string().trim().to_string()),
        "abs" => val(0)
            .as_f64()
            .map(|v| Value::Float(v.abs()))
            .unwrap_or(Value::Null),
        "round" => val(0)
            .as_f64()
            .map(|v| Value::Float(v.round()))
            .unwrap_or(Value::Null),
        "floor" => val(0)
            .as_f64()
            .map(|v| Value::Float(v.floor()))
            .unwrap_or(Value::Null),
        "ceil" => val(0)
            .as_f64()
            .map(|v| Value::Float(v.ceil()))
            .unwrap_or(Value::Null),
        "sqrt" => val(0)
            .as_f64()
            .map(|v| Value::Float(v.sqrt()))
            .unwrap_or(Value::Null),
        "toint" | "tointeger" => val(0).as_i64().map(Value::Int).unwrap_or(Value::Null),
        "tofloat" => val(0).as_f64().map(Value::Float).unwrap_or(Value::Null),
        "tostring" => Value::Text(val(0).to_string()),
        "coalesce" => {
            for i in 0..args.len() {
                let v = val(i);
                if !matches!(v, Value::Null) {
                    return v;
                }
            }
            Value::Null
        }
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------- algorithms

struct Args<'a>(&'a [(String, Value)]);

impl<'a> Args<'a> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    fn f64(&self, key: &str, default: f64) -> f64 {
        self.get(key).and_then(|v| v.as_f64()).unwrap_or(default)
    }
    fn u32(&self, key: &str, default: u32) -> u32 {
        self.get(key)
            .and_then(|v| v.as_i64())
            .map(|v| v.max(0) as u32)
            .unwrap_or(default)
    }
    fn u64(&self, key: &str) -> Option<u64> {
        self.get(key)
            .and_then(|v| v.as_i64())
            .map(|v| v.max(0) as u64)
    }
    fn str(&self, key: &str) -> Option<String> {
        self.get(key).map(|v| v.to_string())
    }
    fn bool(&self, key: &str, default: bool) -> bool {
        self.get(key).map(|v| v.truthy()).unwrap_or(default)
    }
    fn top(&self) -> usize {
        self.get("top")
            .or_else(|| self.get("limit"))
            .and_then(|v| v.as_i64())
            .map(|v| v.max(0) as usize)
            .unwrap_or(usize::MAX)
    }
}

fn call_algorithm(g: &mut Graph, name: &str, args: &[(String, Value)]) -> Result<QueryResult> {
    // Algorithms hold their state within the graph's algorithm memory and
    // spill past it; the projection itself is in memory only when it fits.
    let budget = g.algorithm_memory();
    let tmp = g.temp_dir();
    let r = crate::ooc::with_budget(budget, tmp, || run_algorithm(g, name, args));
    if r.is_ok() {
        if let Some(e) = g.integrity_error() {
            return Err(Error::Msg(format!("storage error during algorithm: {e}")));
        }
    }
    r
}

/// Per-node results, streamed: written back when `write:` is given, and the
/// top rows (all of them without `top:`) returned, highest first.
fn scalar_rows(
    g: &mut Graph,
    mut ids: StateVec<u64>,
    value: &mut dyn FnMut(usize) -> f64,
    col: &str,
    a: &Args,
    integral: bool,
) -> Result<QueryResult> {
    let as_value = |v: f64| {
        if integral {
            Value::Int(v as i64)
        } else {
            Value::Float(v)
        }
    };
    let n = ids.len();
    let write = a.str("write");
    let top = a.top();
    // Best first: higher value, then lower id.
    let better = |x: &(u64, f64), y: &(u64, f64)| y.1.total_cmp(&x.1).then_with(|| x.0.cmp(&y.0));
    let mut keep: Vec<(u64, f64)> = Vec::new();
    for i in 0..n {
        let id = ids.get(i);
        let v = value(i);
        if let Some(prop) = &write {
            g.set_node_prop(id, prop, as_value(v))?;
        }
        if top == 0 {
            continue;
        }
        keep.push((id, v));
        // Bounded top-k: compact once the buffer is twice the limit.
        if top != usize::MAX && keep.len() >= top.saturating_mul(2).max(1024) {
            keep.sort_by(better);
            keep.truncate(top);
        }
    }
    keep.sort_by(better);
    keep.truncate(top);
    let rows: Vec<Vec<Value>> = keep
        .into_iter()
        .map(|(id, v)| vec![Value::Int(id as i64), label_of(g, id), as_value(v)])
        .collect();
    Ok(QueryResult::table(
        vec!["id".into(), "node".into(), col.into()],
        rows,
    ))
}

fn run_algorithm(g: &mut Graph, name: &str, args: &[(String, Value)]) -> Result<QueryResult> {
    let a = Args(args);
    let name = name.to_lowercase();
    let dir = match a.str("dir") {
        Some(s) => Dir::parse(&s).ok_or_else(|| Error::Msg(format!("bad dir '{}'", s)))?,
        None => default_dir(&name),
    };
    let etype = match a.str("type") {
        Some(t) => match g.strings.lookup(&t) {
            Some(id) => Some(id),
            // Unknown type: nothing can match, so run on an empty projection.
            None => Some(u32::MAX),
        },
        None => None,
    };
    let tier = match a.str("tier") {
        Some(t) => Tier::parse(&t).ok_or_else(|| Error::Msg(format!("bad tier '{t}': use mem, ooc or auto")))?,
        None => Tier::Auto,
    };
    let weight = a.str("weight");
    fn proj<'g>(g: &'g Graph, d: Dir, etype: Option<u32>, w: Option<&str>, tier: Tier) -> AlgoView<'g> {
        g.projection(d, etype, w, tier)
    }
    macro_rules! proj {
        ($d:expr, $w:expr) => {
            proj(g, $d, etype, $w, tier)
        };
    }

    match name.as_str() {
        "pagerank" => {
            let p = proj!(dir, weight.as_deref());
            let note = if p.is_paged() { " (out of core)" } else { "" };
            let mut pr = algo::pagerank(
                &p,
                a.f64("damping", 0.85),
                a.u32("iterations", 20),
                a.f64("tolerance", 1e-6),
            );
            let ids = p.into_ids();
            let mut r = scalar_rows(g, ids, &mut |i| pr.scores.get(i), "score", &a, false)?;
            r.message = Some(format!(
                "converged after {} iterations (delta {:.3e}){note}",
                pr.iterations, pr.delta
            ));
            Ok(r)
        }
        "betweenness" => {
            let p = proj!(dir, weight.as_deref());
            let n = p.len();
            let samples = a.u32("samples", 0) as usize;
            let sources: Option<Vec<usize>> = if samples > 0 && samples < n {
                let step = (n / samples).max(1);
                Some((0..n).step_by(step).collect())
            } else {
                None
            };
            let mut scores = {
                let rev = p.reversed();
                algo::betweenness_with(&p, &rev, sources.as_deref(), a.bool("normalize", true))
            };
            let ids = p.into_ids();
            scalar_rows(g, ids, &mut |i| scores.get(i), "betweenness", &a, false)
        }
        "closeness" => {
            let p = proj!(dir, weight.as_deref());
            let mut scores = algo::closeness(&p, weight.is_some());
            let ids = p.into_ids();
            scalar_rows(g, ids, &mut |i| scores.get(i), "closeness", &a, false)
        }
        "degree" => {
            let p = proj!(dir, weight.as_deref());
            let mut deg = StateVec::new(p.len(), 0u64);
            for v in 0..p.len() {
                deg.set(v, p.degree(v) as u64);
            }
            let ids = p.into_ids();
            scalar_rows(g, ids, &mut |i| deg.get(i) as f64, "degree", &a, true)
        }
        "triangles" => {
            let p = proj!(Dir::Both, None);
            let (mut counts, total) = algo::triangles(&p);
            let ids = p.into_ids();
            let mut r = scalar_rows(g, ids, &mut |i| counts.get(i) as f64, "triangles", &a, true)?;
            r.message = Some(format!("{} triangles in total", total));
            Ok(r)
        }
        "clustering" => {
            let p = proj!(Dir::Both, None);
            let (mut counts, _) = algo::triangles(&p);
            let mut coeffs = algo::clustering(&p, &mut counts);
            drop(counts);
            let ids = p.into_ids();
            scalar_rows(g, ids, &mut |i| coeffs.get(i), "clustering", &a, false)
        }
        "kcore" => {
            let p = proj!(Dir::Both, None);
            let mut cores = algo::core_numbers(&p);
            let ids = p.into_ids();
            scalar_rows(g, ids, &mut |i| cores.get(i) as f64, "core", &a, true)
        }
        "components" | "wcc" => {
            let p = proj!(Dir::Both, None);
            let (mut comp, count) = algo::components(&p);
            let ids = p.into_ids();
            let mut r = scalar_rows(g, ids, &mut |i| comp.get(i) as f64, "component", &a, true)?;
            r.message = Some(format!("{} connected components", count));
            Ok(r)
        }
        "scc" => {
            let p = proj!(dir, weight.as_deref());
            let (mut comp, count) = algo::strongly_connected(&p);
            let ids = p.into_ids();
            let mut r = scalar_rows(g, ids, &mut |i| comp.get(i) as f64, "component", &a, true)?;
            r.message = Some(format!("{} strongly connected components", count));
            Ok(r)
        }
        "communities" | "labelprop" => {
            let p = proj!(Dir::Both, None);
            let (mut labels, count) = algo::label_propagation(&p, a.u32("iterations", 20));
            let ids = p.into_ids();
            let mut r = scalar_rows(g, ids, &mut |i| labels.get(i) as f64, "community", &a, true)?;
            r.message = Some(format!("{} communities", count));
            Ok(r)
        }
        "shortestpath" | "path" => {
            let from = a
                .u64("from")
                .ok_or_else(|| Error::Msg("shortestpath needs from:".into()))?;
            let to = a
                .u64("to")
                .ok_or_else(|| Error::Msg("shortestpath needs to:".into()))?;
            // Unweighted: search from both ends over the adjacency, touching
            // only what the search reaches. Weighted, or past the memory
            // budget, falls through to the whole-graph algorithm.
            if weight.is_none() && tier == Tier::Auto {
                if g.node(from).is_none() || g.node(to).is_none() {
                    return Err(Error::Msg("from/to must be existing node ids".into()));
                }
                if let Some(found) = crate::traverse::shortest_path(g, from, to, dir, etype, walk_cap(g)) {
                    return Ok(match found {
                        None => QueryResult::message("no path"),
                        Some(path) => {
                            let hops = path.len() - 1;
                            let rows = path
                                .into_iter()
                                .enumerate()
                                .map(|(i, id)| vec![Value::Int(i as i64), Value::Int(id as i64), label_of(g, id)])
                                .collect();
                            let mut r = QueryResult::table(vec!["step".into(), "id".into(), "node".into()], rows);
                            r.message = Some(format!("{hops} hops, cost {hops}"));
                            r
                        }
                    });
                }
            }
            let p = proj!(dir, weight.as_deref());
            let (Some(s), Some(t)) = (p.index_of(from), p.index_of(to)) else {
                return Err(Error::Msg("from/to must be existing node ids".into()));
            };
            if weight.is_some() && algo::has_negative_weights(&p) {
                return Err(Error::Msg(
                    "negative edge weights are not supported by dijkstra".into(),
                ));
            }
            let mut paths = if weight.is_some() {
                algo::dijkstra(&p, s)
            } else {
                algo::bfs_paths(&p, s)
            };
            match paths.path_to(t) {
                None => Ok(QueryResult::message("no path")),
                Some(path) => {
                    let rows: Vec<Vec<Value>> = path
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let id = p.id(*v);
                            vec![Value::Int(i as i64), Value::Int(id as i64), label_of(g, id)]
                        })
                        .collect();
                    let mut r =
                        QueryResult::table(vec!["step".into(), "id".into(), "node".into()], rows);
                    r.message = Some(format!(
                        "{} hops, cost {}",
                        path.len().saturating_sub(1),
                        paths.dist.get(t)
                    ));
                    Ok(r)
                }
            }
        }
        "sssp" | "distances" => {
            let from = a
                .u64("from")
                .ok_or_else(|| Error::Msg("sssp needs from:".into()))?;
            let p = proj!(dir, weight.as_deref());
            let s = p
                .index_of(from)
                .ok_or_else(|| Error::Msg("from: must be an existing node id".into()))?;
            let paths = if weight.is_some() {
                algo::dijkstra(&p, s)
            } else {
                algo::bfs_paths(&p, s)
            };
            let mut dist = paths.dist;
            drop(paths.parent);
            let mut ids = p.into_ids();
            let write = a.str("write");
            let top = a.top();
            // Nearest first: lower distance, then lower id.
            let better = |x: &(u64, f64), y: &(u64, f64)| x.1.total_cmp(&y.1).then_with(|| x.0.cmp(&y.0));
            let mut keep: Vec<(u64, f64)> = Vec::new();
            for i in 0..ids.len() {
                let d = dist.get(i);
                if !d.is_finite() {
                    continue;
                }
                let id = ids.get(i);
                if let Some(prop) = &write {
                    g.set_node_prop(id, prop, Value::Float(d))?;
                }
                if top == 0 {
                    continue;
                }
                keep.push((id, d));
                if top != usize::MAX && keep.len() >= top.saturating_mul(2).max(1024) {
                    keep.sort_by(better);
                    keep.truncate(top);
                }
            }
            keep.sort_by(better);
            keep.truncate(top);
            let rows = keep
                .into_iter()
                .map(|(id, d)| vec![Value::Int(id as i64), label_of(g, id), Value::Float(d)])
                .collect();
            Ok(QueryResult::table(
                vec!["id".into(), "node".into(), "distance".into()],
                rows,
            ))
        }
        "bfs" | "dfs" => {
            let from = a
                .u64("from")
                .ok_or_else(|| Error::Msg("traversal needs from:".into()))?;
            if tier == Tier::Auto {
                if let Some(r) = local_walk(g, &name, from, dir, etype, &a)? {
                    return Ok(r);
                }
            }
            let p = proj!(dir, weight.as_deref());
            let s = p
                .index_of(from)
                .ok_or_else(|| Error::Msg("from: must be an existing node id".into()))?;
            let depth = a.get("depth").and_then(|v| v.as_i64()).map(|v| v as u32);
            // Stop the walk once `top` visits are in hand.
            let top = a.top();
            let mut visits = Vec::new();
            let mut keep = |v: algo::Visit| {
                if visits.len() < top {
                    visits.push(v);
                }
                visits.len() < top
            };
            if top > 0 {
                if name == "bfs" {
                    algo::bfs_each(&p, s, depth, &mut keep);
                } else {
                    algo::dfs_each(&p, s, depth, &mut keep);
                }
            }
            let rows = visits
                .into_iter()
                .map(|v| {
                    let id = p.id(v.node);
                    vec![
                        Value::Int(id as i64),
                        label_of(g, id),
                        Value::Int(v.depth as i64),
                        v.parent
                            .map(|q| Value::Int(p.id(q) as i64))
                            .unwrap_or(Value::Null),
                    ]
                })
                .collect();
            Ok(QueryResult::table(
                vec!["id".into(), "node".into(), "depth".into(), "parent".into()],
                rows,
            ))
        }
        "neighbors" | "neighbours" => {
            let from = a
                .u64("from")
                .ok_or_else(|| Error::Msg("neighbors needs from:".into()))?;
            let rows: Vec<Vec<Value>> = g
                .neighbors(from, dir, etype)
                .into_iter()
                .take(a.top())
                .map(|adj| {
                    vec![
                        Value::Int(adj.other as i64),
                        label_of(g, adj.other),
                        Value::Text(g.strings.name(adj.etype).to_string()),
                        Value::Int(adj.edge as i64),
                    ]
                })
                .collect();
            Ok(QueryResult::table(
                vec!["id".into(), "node".into(), "type".into(), "edge".into()],
                rows,
            ))
        }
        "subgraph" => {
            let from = a
                .u64("from")
                .ok_or_else(|| Error::Msg("subgraph needs from:".into()))?;
            let depth = a.u32("depth", 2);
            if g.node(from).is_none() {
                return Err(Error::Msg("from: must be an existing node id".into()));
            }
            let top = a.top();
            let mut rows = Vec::new();
            let cap = if tier == Tier::Auto { walk_cap(g) } else { 0 };
            let done = crate::traverse::bfs(g, from, dir, etype, Some(depth), cap, &mut |v| {
                if v.depth > 0 {
                    if rows.len() >= top {
                        return false;
                    }
                    rows.push(vec![Value::Int(v.node as i64), label_of(g, v.node), Value::Int(v.depth as i64)]);
                }
                true
            });
            if done.is_some() {
                return Ok(QueryResult::table(vec!["id".into(), "node".into(), "depth".into()], rows));
            }
            let p = proj!(dir, weight.as_deref());
            let s = p
                .index_of(from)
                .ok_or_else(|| Error::Msg("from: must be an existing node id".into()))?;
            let reach = algo::k_hop(&p, s, a.u32("depth", 2));
            let rows = reach
                .into_iter()
                .take(a.top())
                .map(|(v, d)| {
                    let id = p.id(v);
                    vec![Value::Int(id as i64), label_of(g, id), Value::Int(d as i64)]
                })
                .collect();
            Ok(QueryResult::table(
                vec!["id".into(), "node".into(), "depth".into()],
                rows,
            ))
        }
        "toposort" | "topo" => {
            let p = proj!(dir, weight.as_deref());
            match algo::topological_sort(&p) {
                None => Err(Error::Msg("graph has a cycle: no topological order".into())),
                Some(mut order) => {
                    let rows = (0..order.len())
                        .take(a.top())
                        .map(|i| {
                            let id = p.id(order.get(i) as usize);
                            vec![Value::Int(i as i64), Value::Int(id as i64), label_of(g, id)]
                        })
                        .collect();
                    Ok(QueryResult::table(
                        vec!["position".into(), "id".into(), "node".into()],
                        rows,
                    ))
                }
            }
        }
        "cycle" | "cycles" => {
            let p = proj!(dir, weight.as_deref());
            match algo::find_cycle(&p) {
                None => Ok(QueryResult::message("no cycle found")),
                Some(cycle) => {
                    let rows = cycle
                        .into_iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let id = p.id(v);
                            vec![Value::Int(i as i64), Value::Int(id as i64), label_of(g, id)]
                        })
                        .collect();
                    Ok(QueryResult::table(
                        vec!["step".into(), "id".into(), "node".into()],
                        rows,
                    ))
                }
            }
        }
        "mst" => {
            let p = proj!(Dir::Both, weight.as_deref());
            let (edges, total) = algo::minimum_spanning_forest(&p);
            drop(p);
            let rows: Vec<Vec<Value>> = edges
                .iter()
                .take(a.top())
                .map(|eid| {
                    let e = g.edge(*eid);
                    vec![
                        Value::Int(*eid as i64),
                        Value::Int(e.as_ref().map(|e| e.from as i64).unwrap_or(0)),
                        Value::Int(e.as_ref().map(|e| e.to as i64).unwrap_or(0)),
                        Value::Text(g.edge_type_name(*eid).unwrap_or("").to_string()),
                    ]
                })
                .collect();
            let mut r = QueryResult::table(
                vec!["edge".into(), "from".into(), "to".into(), "type".into()],
                rows,
            );
            r.message = Some(format!("{} edges, total weight {}", edges.len(), total));
            Ok(r)
        }
        other => Err(Error::Msg(format!(
            "unknown algorithm '{}'. try HELP for the list",
            other
        ))),
    }
}

/// How many nodes a local walk may remember before it gives way to the
/// whole-graph algorithm, from the algorithm memory budget.
fn walk_cap(g: &Graph) -> usize {
    (g.algorithm_memory() / crate::traverse::BYTES_PER_NODE).max(1 << 16) as usize
}

/// BFS or DFS over the adjacency, keeping state only for what it visits.
/// `None` when the walk outgrew its budget and the caller should use the
/// whole-graph version.
fn local_walk(g: &Graph, name: &str, from: u64, dir: Dir, etype: Option<u32>, a: &Args) -> Result<Option<QueryResult>> {
    if g.node(from).is_none() {
        return Err(Error::Msg("from: must be an existing node id".into()));
    }
    let depth = a.get("depth").and_then(|v| v.as_i64()).map(|v| v as u32);
    let top = a.top();
    let mut visits = Vec::new();
    if top > 0 {
        let mut keep = |v: crate::traverse::Visit| {
            visits.push(v);
            visits.len() < top
        };
        let walk = if name == "bfs" { crate::traverse::bfs } else { crate::traverse::dfs };
        if walk(g, from, dir, etype, depth, walk_cap(g), &mut keep).is_none() {
            return Ok(None);
        }
    }
    let rows = visits
        .into_iter()
        .map(|v| {
            vec![
                Value::Int(v.node as i64),
                label_of(g, v.node),
                Value::Int(v.depth as i64),
                v.parent.map(|q| Value::Int(q as i64)).unwrap_or(Value::Null),
            ]
        })
        .collect();
    Ok(Some(QueryResult::table(
        vec!["id".into(), "node".into(), "depth".into(), "parent".into()],
        rows,
    )))
}

fn default_dir(name: &str) -> Dir {
    match name {
        "triangles" | "clustering" | "kcore" | "components" | "wcc" | "communities"
        | "labelprop" | "mst" => Dir::Both,
        _ => Dir::Out,
    }
}

/// A short human label for a node: its `name`/`title`/`id` property if present,
/// otherwise its first label.
fn label_of(g: &Graph, id: u64) -> Value {
    for key in ["name", "title", "label", "key"] {
        if let Some(v) = g.node_prop(id, key) {
            return v;
        }
    }
    match g.node_labels(id).first() {
        Some(l) => Value::Text(format!(":{}", l)),
        None => Value::Int(id as i64),
    }
}

// ------------------------------------------------------------- import/export

/// Load JSON Lines: one `{"type":"node"|"edge", ...}` object per line.
pub fn import_jsonl(g: &mut Graph, text: &str) -> Result<(usize, usize)> {
    let was_auto = g.autocommit;
    g.autocommit = false;
    let mut id_map: HashMap<String, u64> = HashMap::new();
    let mut nodes = 0;
    let mut edges = 0;

    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let j = parse_json(line).map_err(|e| Error::Msg(format!("line {}: {}", lineno + 1, e)))?;
        let kind = j.get("type").and_then(|v| v.as_str()).unwrap_or("node");
        let props: Vec<(String, Value)> = match j.get("props") {
            Some(crate::value::Json::Object(fields)) => fields
                .iter()
                .map(|(k, v)| (k.clone(), v.to_value()))
                .collect(),
            _ => Vec::new(),
        };
        if kind == "edge" || kind == "rel" || kind == "relationship" {
            let from = endpoint(&j, "from", &id_map)
                .ok_or_else(|| Error::Msg(format!("line {}: bad 'from'", lineno + 1)))?;
            let to = endpoint(&j, "to", &id_map)
                .ok_or_else(|| Error::Msg(format!("line {}: bad 'to'", lineno + 1)))?;
            let etype = j
                .get("label")
                .or_else(|| j.get("etype"))
                .and_then(|v| v.as_str())
                .unwrap_or("RELATED");
            g.add_edge(from, to, etype, props)?;
            edges += 1;
        } else {
            let labels: Vec<String> = match j.get("labels") {
                Some(crate::value::Json::Array(a)) => a
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                _ => j
                    .get("label")
                    .and_then(|v| v.as_str())
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default(),
            };
            let id = g.add_node(&labels, props)?;
            nodes += 1;
            if let Some(key) = j.get("key").and_then(|v| v.as_str()) {
                id_map.insert(key.to_string(), id);
            }
            if let Some(n) = j.get("id").and_then(|v| v.as_u64()) {
                id_map.insert(n.to_string(), id);
            }
        }
        if (nodes + edges) % 20_000 == 0 {
            g.commit()?;
        }
    }
    g.commit()?;
    g.autocommit = was_auto;
    Ok((nodes, edges))
}

fn endpoint(j: &crate::value::Json, key: &str, map: &HashMap<String, u64>) -> Option<u64> {
    let v = j.get(key)?;
    if let Some(s) = v.as_str() {
        return map.get(s).copied();
    }
    if let Some(n) = v.as_u64() {
        return map.get(&n.to_string()).copied().or(Some(n));
    }
    None
}

/// Dump the whole graph as JSON Lines, re-importable by `import_jsonl`.
/// Holds the whole dump in memory; use [`export_jsonl_to`] for large graphs.
pub fn export_jsonl(g: &Graph) -> String {
    let mut out = Vec::new();
    export_jsonl_to(g, &mut out).expect("writing to a Vec cannot fail");
    String::from_utf8(out).expect("export writes UTF-8")
}

/// Stream the JSON Lines dump to `w`. Pages through node and edge ids, so
/// memory stays flat however large the graph is.
pub fn export_jsonl_to(g: &Graph, w: &mut impl std::io::Write) -> std::io::Result<()> {
    const PAGE: usize = 4096;
    let mut out = String::new();
    let mut from = 0;
    loop {
        let ids = g.nodes_from(from, PAGE);
        let Some(&last) = ids.last() else { break };
        for id in ids {
            write_node_line(g, id, &mut out);
        }
        w.write_all(out.as_bytes())?;
        out.clear();
        from = last + 1;
    }
    from = 0;
    loop {
        let ids = g.edges_from(from, PAGE);
        let Some(&last) = ids.last() else { break };
        for id in ids {
            write_edge_line(g, id, &mut out);
        }
        w.write_all(out.as_bytes())?;
        out.clear();
        from = last + 1;
    }
    w.flush()
}

fn write_node_line(g: &Graph, id: u64, out: &mut String) {
    out.push_str("{\"type\":\"node\",\"id\":");
    out.push_str(&id.to_string());
    out.push_str(",\"labels\":[");
    for (i, l) in g.node_labels(id).iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(l, out);
    }
    out.push_str("],\"props\":{");
    for (i, (k, v)) in g.node_props(id).iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(k, out);
        out.push(':');
        v.write_json(out);
    }
    out.push_str("}}\n");
}

fn write_edge_line(g: &Graph, id: u64, out: &mut String) {
    let Some(e) = g.edge(id) else { return };
    out.push_str("{\"type\":\"edge\",\"id\":");
    out.push_str(&id.to_string());
    out.push_str(",\"label\":");
    write_json_string(g.edge_type_name(id).unwrap_or(""), out);
    out.push_str(&format!(",\"from\":{},\"to\":{}", e.from, e.to));
    out.push_str(",\"props\":{");
    for (i, (k, v)) in g.edge_props(id).iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(k, out);
        out.push(':');
        v.write_json(out);
    }
    out.push_str("}}\n");
}
