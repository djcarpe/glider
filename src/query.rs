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

use std::collections::HashMap;

use crate::algo;
use crate::graph::{Dir, Error, Graph, Result};
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

fn lex(src: &str) -> Result<Vec<Tok>> {
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
    var: Option<String>,
    labels: Vec<String>,
    props: Vec<(String, Expr)>,
}

#[derive(Clone, Debug)]
struct RelPat {
    var: Option<String>,
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
    Help,
}

fn parse(src: &str) -> Result<Stmt> {
    let toks = lex(src)?;
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
                rel.var = Some(v);
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
        pat.var = Some(v);
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
            let (x, y) = (const_value(a)?, const_value(b)?);
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

type Binds = Vec<(String, Bind)>;

fn lookup(binds: &Binds, var: &str) -> Option<Bind> {
    binds.iter().find(|(k, _)| k == var).map(|(_, v)| *v)
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
    let stmt = parse(src)?;
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
            g.commit()?;
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
            let rows = vec![
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
            ];
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
  STATS   SCHEMA   COMPACT   CLEAR   BEGIN   COMMIT

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
            let var = pat.var.clone().unwrap_or_else(|| format!("_{i}"));
            if pat.labels.is_empty() {
                format!("({var})")
            } else {
                format!("({var}:{})", pat.labels.join(":"))
            }
        };

        rows.push(vec![
            Value::Int(pi as i64),
            Value::Text("anchor".into()),
            Value::Text(name(plan.anchor)),
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
    let mut rows: Vec<Binds> = vec![Vec::new()];
    for chain in patterns {
        let mut next: Vec<Binds> = Vec::new();
        for binds in rows {
            match_chain_filtered(g, chain, binds, filter, &mut next)?;
        }
        rows = next;
    }
    if let Some(f) = filter {
        rows.retain(|b| eval(g, f, b).truthy());
    }

    match tail {
        Tail::Count => Ok(QueryResult::message(format!("{} matches", rows.len()))),
        Tail::Return {
            kind,
            distinct,
            order,
            skip,
            limit,
        } => project(g, rows, kind, *distinct, order, *skip, *limit),
        Tail::Set(items) => {
            let mut touched = 0;
            for binds in &rows {
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
            g.commit()?;
            let mut r = QueryResult::message(format!("set {} values", touched));
            r.touched = touched;
            Ok(r)
        }
        Tail::Remove(items) => {
            let mut touched = 0;
            for binds in &rows {
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
            g.commit()?;
            let mut r = QueryResult::message(format!("removed {} values", touched));
            r.touched = touched;
            Ok(r)
        }
        Tail::Delete(vars, _detach) => {
            let mut nodes = Vec::new();
            let mut edges = Vec::new();
            for binds in &rows {
                for var in vars {
                    match lookup(binds, var) {
                        Some(Bind::Node(id)) => nodes.push(id),
                        Some(Bind::Edge(id)) => edges.push(id),
                        None => return Err(Error::Msg(format!("unknown variable {}", var))),
                    }
                }
            }
            nodes.sort_unstable();
            nodes.dedup();
            edges.sort_unstable();
            edges.dedup();
            let mut deleted = 0;
            for id in edges {
                if g.delete_edge(id)? {
                    deleted += 1;
                }
            }
            for id in nodes {
                if g.delete_node(id)? {
                    deleted += 1;
                }
            }
            g.commit()?;
            let mut r = QueryResult::message(format!("deleted {} entities", deleted));
            r.touched = deleted;
            Ok(r)
        }
        Tail::Create(chains) => {
            let mut created_n = 0;
            let mut created_e = 0;
            for binds in rows.iter() {
                let mut b = binds.clone();
                let (n, e) = create_chains(g, chains, &mut b)?;
                created_n += n;
                created_e += e;
            }
            g.commit()?;
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
        if pins.iter().any(|(name, _)| name == v) {
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

    // Outward from the anchor: right as written, then left with directions
    // flipped. Every step starts from a node bound by an earlier step.
    let mut steps = Vec::with_capacity(chain.rels.len());
    for i in anchor..chain.rels.len() {
        steps.push(Step {
            from: i,
            rel: i,
            to: i + 1,
            reversed: false,
        });
    }
    for i in (0..anchor).rev() {
        steps.push(Step {
            from: i + 1,
            rel: i,
            to: i,
            reversed: true,
        });
    }

    Plan {
        anchor,
        estimate: best,
        reason,
        steps,
    }
}

fn match_chain_filtered(
    g: &Graph,
    chain: &Chain,
    binds: Binds,
    filter: Option<&Expr>,
    out: &mut Vec<Binds>,
) -> Result<()> {
    let mut pins = Vec::new();
    pinned_ids(filter, &mut pins);
    let plan = plan_chain(g, chain, &binds, &pins);
    let pat = &chain.nodes[plan.anchor];

    for id in anchor_candidates(g, pat, &binds, &pins) {
        if !node_matches(g, pat, id, &binds) {
            continue;
        }
        let mut b = binds.clone();
        if let Some(v) = &pat.var {
            match lookup(&b, v) {
                Some(existing) if existing != Bind::Node(id) => continue,
                Some(_) => {}
                None => b.push((v.clone(), Bind::Node(id))),
            }
        }
        let mut bound = vec![None; chain.nodes.len()];
        bound[plan.anchor] = Some(id);
        walk(g, chain, &plan.steps, 0, b, bound, out)?;
    }
    Ok(())
}

/// Enumerate the anchor's candidate set, cheapest source first.
fn anchor_candidates(g: &Graph, pat: &NodePat, binds: &Binds, pins: &[(String, u64)]) -> Vec<u64> {
    if let Some(v) = &pat.var {
        if let Some(Bind::Node(id)) = lookup(binds, v) {
            return vec![id];
        }
        if let Some((_, id)) = pins.iter().find(|(name, _)| name == v) {
            return if g.node(*id).is_some() {
                vec![*id]
            } else {
                Vec::new()
            };
        }
    }
    candidate_nodes(g, pat, binds)
}

/// Execute the plan, one step at a time, backtracking on failure.
fn walk(
    g: &Graph,
    chain: &Chain,
    steps: &[Step],
    i: usize,
    binds: Binds,
    bound: Vec<Option<u64>>,
    out: &mut Vec<Binds>,
) -> Result<()> {
    if i >= steps.len() {
        out.push(binds);
        return Ok(());
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
        None => return Ok(()),
    };

    let type_ids: Vec<u32> = rel
        .types
        .iter()
        .filter_map(|t| g.strings.lookup(t))
        .collect();
    if !rel.types.is_empty() && type_ids.len() != rel.types.len() {
        return Ok(());
    }

    if let Some((min, max)) = rel.hops {
        let mut frontier = vec![current];
        let mut seen = crate::graph::id_set();
        seen.insert(current);
        for depth in 1..=max {
            let mut next = Vec::new();
            for node in &frontier {
                for adj in g.neighbors(*node, dir, None) {
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
                    walk(g, chain, steps, i + 1, b, bound2, out)?;
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        return Ok(());
    }

    for adj in g.neighbors(current, dir, None) {
        if !type_ids.is_empty() && !type_ids.contains(&adj.etype) {
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
        walk(g, chain, steps, i + 1, b, bound2, out)?;
    }
    Ok(())
}

fn candidate_nodes(g: &Graph, pat: &NodePat, binds: &Binds) -> Vec<u64> {
    if let Some(v) = &pat.var {
        if let Some(Bind::Node(id)) = lookup(binds, v) {
            return vec![id];
        }
    }
    for label in &pat.labels {
        for (key, expr) in &pat.props {
            if let Expr::Lit(v) = expr {
                if g.has_index(label, key) {
                    if let Some(ids) = g.indexed_lookup(label, key, v) {
                        return ids;
                    }
                }
            }
        }
    }
    if let Some(label) = pat.labels.first() {
        return g.nodes_with_label(label);
    }
    g.node_ids()
}

fn node_matches(g: &Graph, pat: &NodePat, id: u64, binds: &Binds) -> bool {
    if g.node(id).is_none() {
        return false;
    }
    for label in &pat.labels {
        match g.strings.lookup(label) {
            Some(l) if g.has_label(id, l) => {}
            _ => return false,
        }
    }
    for (key, expr) in &pat.props {
        let want = eval(g, expr, binds);
        match g.node_prop(id, key) {
            Some(got) if *got == want => {}
            _ => return false,
        }
    }
    true
}

fn edge_props_match(g: &Graph, rel: &RelPat, edge: u64, binds: &Binds) -> bool {
    for (key, expr) in &rel.props {
        let want = eval(g, expr, binds);
        match g.edge_prop(edge, key) {
            Some(got) if *got == want => {}
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

fn project(
    g: &Graph,
    rows: Vec<Binds>,
    kind: &ReturnKind,
    distinct: bool,
    order: &[(Expr, bool)],
    skip: usize,
    limit: Option<usize>,
) -> Result<QueryResult> {
    let items: Vec<(Expr, String)> = match kind {
        ReturnKind::Items(items) => items.clone(),
        ReturnKind::All => {
            let mut vars: Vec<String> = Vec::new();
            for b in &rows {
                for (k, _) in b {
                    if !vars.contains(k) {
                        vars.push(k.clone());
                    }
                }
            }
            vars.into_iter()
                .map(|v| (Expr::Var(v.clone()), v))
                .collect()
        }
    };

    let columns: Vec<String> = items.iter().map(|(_, a)| a.clone()).collect();
    let has_agg = items.iter().any(|(e, _)| e.is_aggregate());

    let mut out_rows: Vec<Vec<Value>> = if has_agg {
        // Implicit grouping on the non-aggregate items, as Cypher does.
        let mut groups: Vec<(Vec<Value>, Vec<Binds>)> = Vec::new();
        for binds in rows {
            let key: Vec<Value> = items
                .iter()
                .filter(|(e, _)| !e.is_aggregate())
                .map(|(e, _)| eval(g, e, &binds))
                .collect();
            match groups.iter_mut().find(|(k, _)| {
                k.len() == key.len() && k.iter().zip(key.iter()).all(|(a, b)| a == b)
            }) {
                Some((_, members)) => members.push(binds),
                None => groups.push((key, vec![binds])),
            }
        }
        groups
            .into_iter()
            .map(|(_, members)| {
                items
                    .iter()
                    .map(|(e, _)| {
                        if e.is_aggregate() {
                            eval_aggregate(g, e, &members)
                        } else {
                            members
                                .first()
                                .map(|b| eval(g, e, b))
                                .unwrap_or(Value::Null)
                        }
                    })
                    .collect()
            })
            .collect()
    } else {
        rows.iter()
            .map(|b| items.iter().map(|(e, _)| eval(g, e, b)).collect())
            .collect()
    };

    if distinct {
        let mut seen: Vec<Vec<Value>> = Vec::new();
        out_rows.retain(|row| {
            if seen.iter().any(|s| s == row) {
                false
            } else {
                seen.push(row.clone());
                true
            }
        });
    }

    if !order.is_empty() {
        let keys: Vec<(usize, bool)> = order
            .iter()
            .map(|(e, desc)| {
                let alias = e.alias();
                let idx = columns.iter().position(|c| *c == alias).ok_or_else(|| {
                    Error::Msg(format!("ORDER BY {} is not a returned column", alias))
                })?;
                Ok((idx, *desc))
            })
            .collect::<Result<Vec<_>>>()?;
        out_rows.sort_by(|a, b| {
            for (idx, desc) in &keys {
                let ord = a[*idx].total_cmp(&b[*idx]);
                let ord = if *desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    let total = out_rows.len();
    let mut final_rows: Vec<Vec<Value>> = out_rows.into_iter().skip(skip).collect();
    if let Some(l) = limit {
        final_rows.truncate(l);
    }

    Ok(QueryResult {
        columns,
        rows: final_rows,
        message: None,
        touched: total,
    })
}

fn eval_aggregate(g: &Graph, e: &Expr, members: &[Binds]) -> Value {
    let Expr::Func(name, args) = e else {
        return Value::Null;
    };
    let arg = args.first();
    let star = matches!(arg, Some(Expr::Lit(Value::Text(s))) if s == "*");
    let values: Vec<Value> = if star {
        members.iter().map(|_| Value::Int(1)).collect()
    } else {
        match arg {
            Some(a) => members
                .iter()
                .map(|b| eval(g, a, b))
                .filter(|v| !matches!(v, Value::Null))
                .collect(),
            None => Vec::new(),
        }
    };
    match name.as_str() {
        "count" => Value::Int(values.len() as i64),
        "sum" => Value::Float(values.iter().filter_map(|v| v.as_f64()).sum()),
        "avg" => {
            let nums: Vec<f64> = values.iter().filter_map(|v| v.as_f64()).collect();
            if nums.is_empty() {
                Value::Null
            } else {
                Value::Float(nums.iter().sum::<f64>() / nums.len() as f64)
            }
        }
        "min" => values
            .into_iter()
            .min_by(|a, b| a.total_cmp(b))
            .unwrap_or(Value::Null),
        "max" => values
            .into_iter()
            .max_by(|a, b| a.total_cmp(b))
            .unwrap_or(Value::Null),
        "collect" => Value::List(values),
        _ => Value::Null,
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
            Some(Bind::Node(id)) => g.node_prop(id, key).cloned().unwrap_or(Value::Null),
            Some(Bind::Edge(id)) => g.edge_prop(id, key).cloned().unwrap_or(Value::Null),
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
    let weight = a.str("weight");
    let csr = g.csr(dir, etype, weight.as_deref());

    let scalar = |g: &mut Graph,
                  values: Vec<f64>,
                  col: &str,
                  a: &Args,
                  integral: bool|
     -> Result<QueryResult> {
        let mut pairs: Vec<(u64, f64)> = csr
            .ids
            .iter()
            .copied()
            .zip(values.iter().copied())
            .collect();
        if let Some(prop) = a.str("write") {
            for (id, v) in &pairs {
                let value = if integral {
                    Value::Int(*v as i64)
                } else {
                    Value::Float(*v)
                };
                g.set_node_prop(*id, &prop, value)?;
            }
            g.commit()?;
        }
        pairs.sort_by(|x, y| y.1.total_cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
        let top = a.top();
        let rows: Vec<Vec<Value>> = pairs
            .into_iter()
            .take(top)
            .map(|(id, v)| {
                vec![
                    Value::Int(id as i64),
                    label_of(g, id),
                    if integral {
                        Value::Int(v as i64)
                    } else {
                        Value::Float(v)
                    },
                ]
            })
            .collect();
        Ok(QueryResult::table(
            vec!["id".into(), "node".into(), col.into()],
            rows,
        ))
    };

    match name.as_str() {
        "pagerank" => {
            let pr = algo::pagerank(
                &csr,
                a.f64("damping", 0.85),
                a.u32("iterations", 20),
                a.f64("tolerance", 1e-6),
            );
            let mut r = scalar(g, pr.scores, "score", &a, false)?;
            r.message = Some(format!(
                "converged after {} iterations (delta {:.3e})",
                pr.iterations, pr.delta
            ));
            Ok(r)
        }
        "betweenness" => {
            let samples = a.u32("samples", 0) as usize;
            let sources: Option<Vec<usize>> = if samples > 0 && samples < csr.len() {
                let step = (csr.len() / samples).max(1);
                Some((0..csr.len()).step_by(step).collect())
            } else {
                None
            };
            let scores = algo::betweenness(&csr, sources.as_deref(), a.bool("normalize", true));
            scalar(g, scores, "betweenness", &a, false)
        }
        "closeness" => {
            let scores = algo::closeness(&csr, weight.is_some());
            scalar(g, scores, "closeness", &a, false)
        }
        "degree" => {
            let scores: Vec<f64> = (0..csr.len()).map(|v| csr.degree(v) as f64).collect();
            scalar(g, scores, "degree", &a, true)
        }
        "triangles" => {
            let both = g.csr(Dir::Both, etype, None);
            let (counts, total) = algo::triangles(&both);
            let mut r = scalar(
                g,
                counts.iter().map(|c| *c as f64).collect(),
                "triangles",
                &a,
                true,
            )?;
            r.message = Some(format!("{} triangles in total", total));
            Ok(r)
        }
        "clustering" => {
            let both = g.csr(Dir::Both, etype, None);
            let (counts, _) = algo::triangles(&both);
            let coeffs = algo::clustering(&both, &counts);
            scalar(g, coeffs, "clustering", &a, false)
        }
        "kcore" => {
            let both = g.csr(Dir::Both, etype, None);
            let cores = algo::core_numbers(&both);
            scalar(
                g,
                cores.iter().map(|c| *c as f64).collect(),
                "core",
                &a,
                true,
            )
        }
        "components" | "wcc" => {
            let both = g.csr(Dir::Both, etype, None);
            let (comp, count) = algo::components(&both);
            let mut r = scalar(
                g,
                comp.iter().map(|c| *c as f64).collect(),
                "component",
                &a,
                true,
            )?;
            r.message = Some(format!("{} connected components", count));
            Ok(r)
        }
        "scc" => {
            let (comp, count) = algo::strongly_connected(&csr);
            let mut r = scalar(
                g,
                comp.iter().map(|c| *c as f64).collect(),
                "component",
                &a,
                true,
            )?;
            r.message = Some(format!("{} strongly connected components", count));
            Ok(r)
        }
        "communities" | "labelprop" => {
            let both = g.csr(Dir::Both, etype, None);
            let (labels, count) = algo::label_propagation(&both, a.u32("iterations", 20));
            let mut r = scalar(
                g,
                labels.iter().map(|l| *l as f64).collect(),
                "community",
                &a,
                true,
            )?;
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
            let (Some(s), Some(t)) = (csr.index_of(from), csr.index_of(to)) else {
                return Err(Error::Msg("from/to must be existing node ids".into()));
            };
            if weight.is_some() && algo::has_negative_weights(&csr) {
                return Err(Error::Msg(
                    "negative edge weights are not supported by dijkstra".into(),
                ));
            }
            let paths = if weight.is_some() {
                algo::dijkstra(&csr, s)
            } else {
                algo::bfs_paths(&csr, s)
            };
            match paths.path_to(t) {
                None => Ok(QueryResult::message("no path")),
                Some(path) => {
                    let rows: Vec<Vec<Value>> = path
                        .iter()
                        .enumerate()
                        .map(|(i, p)| {
                            let id = csr.ids[*p];
                            vec![Value::Int(i as i64), Value::Int(id as i64), label_of(g, id)]
                        })
                        .collect();
                    let mut r =
                        QueryResult::table(vec!["step".into(), "id".into(), "node".into()], rows);
                    r.message = Some(format!(
                        "{} hops, cost {}",
                        path.len().saturating_sub(1),
                        paths.dist[t]
                    ));
                    Ok(r)
                }
            }
        }
        "sssp" | "distances" => {
            let from = a
                .u64("from")
                .ok_or_else(|| Error::Msg("sssp needs from:".into()))?;
            let s = csr
                .index_of(from)
                .ok_or_else(|| Error::Msg("from: must be an existing node id".into()))?;
            let paths = if weight.is_some() {
                algo::dijkstra(&csr, s)
            } else {
                algo::bfs_paths(&csr, s)
            };
            let mut pairs: Vec<(u64, f64)> = csr
                .ids
                .iter()
                .copied()
                .zip(paths.dist.iter().copied())
                .filter(|(_, d)| d.is_finite())
                .collect();
            pairs.sort_by(|x, y| x.1.total_cmp(&y.1).then_with(|| x.0.cmp(&y.0)));
            if let Some(prop) = a.str("write") {
                for (id, d) in &pairs {
                    g.set_node_prop(*id, &prop, Value::Float(*d))?;
                }
                g.commit()?;
            }
            let rows = pairs
                .into_iter()
                .take(a.top())
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
            let s = csr
                .index_of(from)
                .ok_or_else(|| Error::Msg("from: must be an existing node id".into()))?;
            let depth = a.get("depth").and_then(|v| v.as_i64()).map(|v| v as u32);
            let visits = if name == "bfs" {
                algo::bfs(&csr, s, depth)
            } else {
                algo::dfs(&csr, s, depth)
            };
            let rows = visits
                .into_iter()
                .take(a.top())
                .map(|v| {
                    let id = csr.ids[v.node];
                    vec![
                        Value::Int(id as i64),
                        label_of(g, id),
                        Value::Int(v.depth as i64),
                        v.parent
                            .map(|p| Value::Int(csr.ids[p] as i64))
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
                .neighbors(
                    from,
                    dir,
                    if etype == Some(u32::MAX) {
                        etype
                    } else {
                        etype
                    },
                )
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
            let s = csr
                .index_of(from)
                .ok_or_else(|| Error::Msg("from: must be an existing node id".into()))?;
            let reach = algo::k_hop(&csr, s, a.u32("depth", 2));
            let rows = reach
                .into_iter()
                .take(a.top())
                .map(|(v, d)| {
                    let id = csr.ids[v];
                    vec![Value::Int(id as i64), label_of(g, id), Value::Int(d as i64)]
                })
                .collect();
            Ok(QueryResult::table(
                vec!["id".into(), "node".into(), "depth".into()],
                rows,
            ))
        }
        "toposort" | "topo" => match algo::topological_sort(&csr) {
            None => Err(Error::Msg("graph has a cycle: no topological order".into())),
            Some(order) => {
                let rows = order
                    .into_iter()
                    .enumerate()
                    .take(a.top())
                    .map(|(i, v)| {
                        let id = csr.ids[v];
                        vec![Value::Int(i as i64), Value::Int(id as i64), label_of(g, id)]
                    })
                    .collect();
                Ok(QueryResult::table(
                    vec!["position".into(), "id".into(), "node".into()],
                    rows,
                ))
            }
        },
        "cycle" | "cycles" => match algo::find_cycle(&csr) {
            None => Ok(QueryResult::message("no cycle found")),
            Some(cycle) => {
                let rows = cycle
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let id = csr.ids[v];
                        vec![Value::Int(i as i64), Value::Int(id as i64), label_of(g, id)]
                    })
                    .collect();
                Ok(QueryResult::table(
                    vec!["step".into(), "id".into(), "node".into()],
                    rows,
                ))
            }
        },
        "mst" => {
            let both = g.csr(Dir::Both, etype, weight.as_deref());
            let (edges, total) = algo::minimum_spanning_forest(&both);
            let rows: Vec<Vec<Value>> = edges
                .iter()
                .take(a.top())
                .map(|eid| {
                    let e = g.edge(*eid);
                    vec![
                        Value::Int(*eid as i64),
                        Value::Int(e.map(|e| e.from as i64).unwrap_or(0)),
                        Value::Int(e.map(|e| e.to as i64).unwrap_or(0)),
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
            return v.clone();
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
pub fn export_jsonl(g: &Graph) -> String {
    let mut out = String::new();
    for id in g.node_ids() {
        out.push_str("{\"type\":\"node\",\"id\":");
        out.push_str(&id.to_string());
        out.push_str(",\"labels\":[");
        for (i, l) in g.node_labels(id).iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(l, &mut out);
        }
        out.push_str("],\"props\":{");
        for (i, (k, v)) in g.node_props(id).iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(k, &mut out);
            out.push(':');
            v.write_json(&mut out);
        }
        out.push_str("}}\n");
    }
    for id in g.edge_ids() {
        let Some(e) = g.edge(id) else { continue };
        out.push_str("{\"type\":\"edge\",\"id\":");
        out.push_str(&id.to_string());
        out.push_str(",\"label\":");
        write_json_string(g.edge_type_name(id).unwrap_or(""), &mut out);
        out.push_str(&format!(",\"from\":{},\"to\":{}", e.from, e.to));
        out.push_str(",\"props\":{");
        for (i, (k, v)) in g.edge_props(id).iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(k, &mut out);
            out.push(':');
            v.write_json(&mut out);
        }
        out.push_str("}}\n");
    }
    out
}
