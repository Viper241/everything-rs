//! A small backtracking regular-expression engine covering the subset that
//! Everything documents for `regex:` searches:
//!
//! ```text
//! a|b        alternation          gr(a|e)y
//! .          any character        ^ $        anchors
//! [abc] [^abc] [a-z]             character classes
//! \b \B      word boundaries      \d \w \s and negations
//! * ? +      repetition           *? +? ??   lazy repetition
//! {n} {n,} {n,m}                 counted repetition
//! \x         escape
//! ```
//!
//! The engine is a Thompson-style program executed by a backtracking VM with a
//! step budget, so a hostile pattern can never hang the search thread.

use std::sync::Arc;

const MAX_PROG: usize = 60_000;
const STEP_BUDGET: u32 = 250_000;

#[derive(Debug, Clone)]
pub struct Regex {
    prog: Vec<Inst>,
    classes: Vec<Class>,
    /// True when the pattern is anchored at the start, which lets `is_match`
    /// skip the "try every start offset" loop.
    anchored: bool,
}

#[derive(Debug, Clone, Copy)]
enum Inst {
    Char(char),
    Any,
    Class(u16),
    Start,
    End,
    WordBoundary,
    NotWordBoundary,
    Split(usize, usize),
    Jmp(usize),
    Match,
}

#[derive(Debug, Clone, Default)]
struct Class {
    ranges: Vec<(char, char)>,
    negated: bool,
}

impl Class {
    fn matches(&self, c: char) -> bool {
        let hit = self
            .ranges
            .iter()
            .any(|&(lo, hi)| c >= lo && c <= hi);
        hit != self.negated
    }
}

#[derive(Debug, Clone)]
enum Ast {
    Empty,
    Char(char),
    Any,
    Class(Class),
    Start,
    End,
    WordBoundary(bool),
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Repeat {
        inner: Box<Ast>,
        min: u32,
        max: Option<u32>,
        lazy: bool,
    },
}

#[derive(Debug, Clone)]
pub struct RegexError {
    pub message: String,
    pub position: usize,
}

impl std::fmt::Display for RegexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at {})", self.message, self.position)
    }
}

impl std::error::Error for RegexError {}

type Result<T> = std::result::Result<T, RegexError>;

pub fn compile(pattern: &str) -> Result<Arc<Regex>> {
    let mut p = Parser {
        src: pattern.chars().collect(),
        pos: 0,
    };
    let ast = p.parse_alt()?;
    if p.pos < p.src.len() {
        return Err(p.err("unexpected character"));
    }
    let mut c = Compiler {
        prog: Vec::new(),
        classes: Vec::new(),
    };
    c.emit_ast(&ast)?;
    c.prog.push(Inst::Match);
    Ok(Arc::new(Regex {
        prog: c.prog,
        classes: c.classes,
        anchored: matches!(ast, Ast::Start | Ast::Concat(_)) && starts_anchored(&ast),
    }))
}

fn starts_anchored(ast: &Ast) -> bool {
    match ast {
        Ast::Start => true,
        Ast::Concat(items) => items.first().map(starts_anchored).unwrap_or(false),
        _ => false,
    }
}

struct Parser {
    src: Vec<char>,
    pos: usize,
}

impl Parser {
    fn err(&self, msg: &str) -> RegexError {
        RegexError {
            message: msg.to_string(),
            position: self.pos,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_alt(&mut self) -> Result<Ast> {
        let mut parts = vec![self.parse_concat()?];
        while self.eat('|') {
            parts.push(self.parse_concat()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Ast::Alt(parts)
        })
    }

    fn parse_concat(&mut self) -> Result<Ast> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == ')' || c == '|' {
                break;
            }
            items.push(self.parse_repeat()?);
        }
        Ok(match items.len() {
            0 => Ast::Empty,
            1 => items.pop().unwrap(),
            _ => Ast::Concat(items),
        })
    }

    fn parse_repeat(&mut self) -> Result<Ast> {
        let mut atom = self.parse_atom()?;
        loop {
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    let l = self.eat('?');
                    atom = Ast::Repeat {
                        inner: Box::new(atom),
                        min: 0,
                        max: None,
                        lazy: l,
                    };
                }
                Some('+') => {
                    self.pos += 1;
                    let l = self.eat('?');
                    atom = Ast::Repeat {
                        inner: Box::new(atom),
                        min: 1,
                        max: None,
                        lazy: l,
                    };
                }
                Some('?') => {
                    self.pos += 1;
                    let l = self.eat('?');
                    atom = Ast::Repeat {
                        inner: Box::new(atom),
                        min: 0,
                        max: Some(1),
                        lazy: l,
                    };
                }
                Some('{') => {
                    let save = self.pos;
                    match self.parse_counted() {
                        Some((min, max)) => {
                            let l = self.eat('?');
                            atom = Ast::Repeat {
                                inner: Box::new(atom),
                                min,
                                max,
                                lazy: l,
                            };
                        }
                        None => {
                            // Not a valid {n,m} - treat '{' literally next time.
                            self.pos = save;
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(atom)
    }

    fn parse_counted(&mut self) -> Option<(u32, Option<u32>)> {
        debug_assert_eq!(self.peek(), Some('{'));
        self.pos += 1;
        let min = self.parse_int()?;
        let max = if self.eat(',') {
            let m = self.parse_int();
            if self.peek() != Some('}') {
                return None;
            }
            m
        } else {
            if self.peek() != Some('}') {
                return None;
            }
            Some(min)
        };
        self.pos += 1; // '}'
        if let Some(m) = max {
            if m < min {
                return None;
            }
        }
        if min > 1000 || max.unwrap_or(0) > 1000 {
            return None;
        }
        Some((min, max))
    }

    fn parse_int(&mut self) -> Option<u32> {
        let start = self.pos;
        let mut v: u32 = 0;
        while let Some(c) = self.peek() {
            if let Some(d) = c.to_digit(10) {
                v = v.checked_mul(10)?.checked_add(d)?;
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start { None } else { Some(v) }
    }

    fn parse_atom(&mut self) -> Result<Ast> {
        match self.bump() {
            None => Ok(Ast::Empty),
            Some('(') => {
                let inner = self.parse_alt()?;
                if !self.eat(')') {
                    return Err(self.err("missing ')'"));
                }
                Ok(inner)
            }
            Some('[') => Ok(Ast::Class(self.parse_class()?)),
            Some('.') => Ok(Ast::Any),
            Some('^') => Ok(Ast::Start),
            Some('$') => Ok(Ast::End),
            Some('\\') => self.parse_escape(false),
            Some(c) => Ok(Ast::Char(c)),
        }
    }

    fn parse_class(&mut self) -> Result<Class> {
        let mut class = Class::default();
        if self.eat('^') {
            class.negated = true;
        }
        let mut first = true;
        loop {
            let c = match self.bump() {
                None => return Err(self.err("missing ']'")),
                Some(']') if !first => break,
                Some(c) => c,
            };
            first = false;
            let lo = if c == '\\' {
                match self.bump() {
                    Some('d') => {
                        push_digits(&mut class);
                        continue;
                    }
                    Some('w') => {
                        push_word(&mut class);
                        continue;
                    }
                    Some('s') => {
                        push_space(&mut class);
                        continue;
                    }
                    Some('D') => {
                        let mut sub = Class::default();
                        push_digits(&mut sub);
                        sub.negated = true;
                        class.ranges.extend(sub.ranges);
                        continue;
                    }
                    Some('W') => {
                        let mut sub = Class::default();
                        push_word(&mut sub);
                        sub.negated = true;
                        class.ranges.extend(sub.ranges);
                        continue;
                    }
                    Some('S') => {
                        let mut sub = Class::default();
                        push_space(&mut sub);
                        sub.negated = true;
                        class.ranges.extend(sub.ranges);
                        continue;
                    }
                    Some('n') => '\n',
                    Some('t') => '\t',
                    Some('r') => '\r',
                    Some('f') => '\x0c',
                    Some('v') => '\x0b',
                    Some('0') => '\0',
                    Some(other) => other,
                    None => return Err(self.err("trailing '\\'")),
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.src.get(self.pos + 1).is_some_and(|&c| c != ']') {
                self.pos += 1;
                let hi_raw = self.bump().unwrap();
                let hi = if hi_raw == '\\' {
                    self.bump().unwrap_or(hi_raw)
                } else {
                    hi_raw
                };
                class.ranges.push((lo, hi));
            } else {
                class.ranges.push((lo, lo));
            }
        }
        Ok(class)
    }

    fn parse_escape(&mut self, in_class: bool) -> Result<Ast> {
        let c = match self.bump() {
            None => return Err(self.err("trailing '\\'")),
            Some(c) => c,
        };
        Ok(match c {
            'd' => digit_class(),
            'D' => {
                let mut cl = digit_class();
                if let Ast::Class(ref mut c) = cl {
                    c.negated = true;
                }
                cl
            }
            'w' => word_class(),
            'W' => {
                let mut cl = word_class();
                if let Ast::Class(ref mut c) = cl {
                    c.negated = true;
                }
                cl
            }
            's' => space_class(),
            'S' => {
                let mut cl = space_class();
                if let Ast::Class(ref mut c) = cl {
                    c.negated = true;
                }
                cl
            }
            'b' if !in_class => Ast::WordBoundary(true),
            'B' if !in_class => Ast::WordBoundary(false),
            'n' => Ast::Char('\n'),
            't' => Ast::Char('\t'),
            'r' => Ast::Char('\r'),
            'f' => Ast::Char('\x0c'),
            'v' => Ast::Char('\x0b'),
            '0' => Ast::Char('\0'),
            'x' => {
                let mut v = 0u32;
                for _ in 0..2 {
                    let d = self.bump().and_then(|c| c.to_digit(16));
                    match d {
                        Some(d) => v = v * 16 + d,
                        None => return Ok(Ast::Char('x')),
                    }
                }
                Ast::Char(char::from_u32(v).unwrap_or('x'))
            }
            other => Ast::Char(other),
        })
    }
}

fn push_digits(c: &mut Class) {
    c.ranges.push(('0', '9'));
}
fn push_word(c: &mut Class) {
    c.ranges.push(('a', 'z'));
    c.ranges.push(('A', 'Z'));
    c.ranges.push(('0', '9'));
    c.ranges.push(('_', '_'));
}
fn push_space(c: &mut Class) {
    c.ranges.push((' ', ' '));
    c.ranges.push(('\t', '\t'));
    c.ranges.push(('\n', '\n'));
    c.ranges.push(('\r', '\r'));
    c.ranges.push(('\x0b', '\x0c'));
}

fn digit_class() -> Ast {
    let mut c = Class::default();
    push_digits(&mut c);
    Ast::Class(c)
}
fn word_class() -> Ast {
    let mut c = Class::default();
    push_word(&mut c);
    Ast::Class(c)
}
fn space_class() -> Ast {
    let mut c = Class::default();
    push_space(&mut c);
    Ast::Class(c)
}

struct Compiler {
    prog: Vec<Inst>,
    classes: Vec<Class>,
}

impl Compiler {
    fn check(&self) -> Result<()> {
        if self.prog.len() > MAX_PROG {
            return Err(RegexError {
                message: "pattern is too large".into(),
                position: 0,
            });
        }
        Ok(())
    }

    fn emit(&mut self, i: Inst) -> usize {
        self.prog.push(i);
        self.prog.len() - 1
    }

    fn emit_ast(&mut self, ast: &Ast) -> Result<()> {
        self.check()?;
        match ast {
            Ast::Empty => {}
            Ast::Char(c) => {
                self.emit(Inst::Char(*c));
            }
            Ast::Any => {
                self.emit(Inst::Any);
            }
            Ast::Class(c) => {
                let idx = self.classes.len();
                if idx > u16::MAX as usize {
                    return Err(RegexError {
                        message: "too many character classes".into(),
                        position: 0,
                    });
                }
                self.classes.push(c.clone());
                self.emit(Inst::Class(idx as u16));
            }
            Ast::Start => {
                self.emit(Inst::Start);
            }
            Ast::End => {
                self.emit(Inst::End);
            }
            Ast::WordBoundary(b) => {
                self.emit(if *b {
                    Inst::WordBoundary
                } else {
                    Inst::NotWordBoundary
                });
            }
            Ast::Concat(items) => {
                for it in items {
                    self.emit_ast(it)?;
                }
            }
            Ast::Alt(parts) => {
                let mut jumps = Vec::new();
                let mut next = Vec::new();
                let n = parts.len();
                for (i, part) in parts.iter().enumerate() {
                    if i + 1 < n {
                        let split = self.emit(Inst::Split(0, 0));
                        self.emit_ast(part)?;
                        jumps.push(self.emit(Inst::Jmp(0)));
                        let here = self.prog.len();
                        if let Inst::Split(a, b) = &mut self.prog[split] {
                            *a = split + 1;
                            *b = here;
                        }
                        next.push(split);
                    } else {
                        self.emit_ast(part)?;
                    }
                }
                let end = self.prog.len();
                for j in jumps {
                    if let Inst::Jmp(a) = &mut self.prog[j] {
                        *a = end;
                    }
                }
                let _ = next;
            }
            Ast::Repeat {
                inner,
                min,
                max,
                lazy,
            } => {
                for _ in 0..*min {
                    self.emit_ast(inner)?;
                }
                match max {
                    None => {
                        let split = self.emit(Inst::Split(0, 0));
                        self.emit_ast(inner)?;
                        self.emit(Inst::Jmp(split));
                        let end = self.prog.len();
                        if let Inst::Split(a, b) = &mut self.prog[split] {
                            if *lazy {
                                *a = end;
                                *b = split + 1;
                            } else {
                                *a = split + 1;
                                *b = end;
                            }
                        }
                    }
                    Some(max) => {
                        let extra = max.saturating_sub(*min);
                        let mut splits = Vec::new();
                        for _ in 0..extra {
                            splits.push(self.emit(Inst::Split(0, 0)));
                            self.emit_ast(inner)?;
                        }
                        let end = self.prog.len();
                        for s in splits {
                            if let Inst::Split(a, b) = &mut self.prog[s] {
                                if *lazy {
                                    *a = end;
                                    *b = s + 1;
                                } else {
                                    *a = s + 1;
                                    *b = end;
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Regex {
    /// Unanchored search, like a normal regex `test`.
    pub fn is_match(&self, hay: &str) -> bool {
        let chars: Vec<char> = hay.chars().collect();
        if self.anchored {
            return self.run_at(&chars, 0);
        }
        (0..=chars.len()).any(|start| self.run_at(&chars, start))
    }

    fn run_at(&self, chars: &[char], start: usize) -> bool {
        let mut budget = STEP_BUDGET;
        let mut stack: Vec<(usize, usize)> = Vec::with_capacity(32);
        stack.push((0, start));
        while let Some((mut pc, mut pos)) = stack.pop() {
            loop {
                if budget == 0 {
                    return false;
                }
                budget -= 1;
                match self.prog[pc] {
                    Inst::Char(c) => {
                        if chars.get(pos) == Some(&c) {
                            pc += 1;
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Any => {
                        if pos < chars.len() {
                            pc += 1;
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Class(ci) => {
                        if chars.get(pos).is_some_and(|&c| self.classes[ci as usize].matches(c)) {
                            pc += 1;
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Start => {
                        if pos == 0 {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::End => {
                        if pos == chars.len() {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::WordBoundary | Inst::NotWordBoundary => {
                        let before = pos > 0 && is_word(chars[pos - 1]);
                        let after = chars.get(pos).is_some_and(|&c| is_word(c));
                        let boundary = before != after;
                        let want = matches!(self.prog[pc], Inst::WordBoundary);
                        if boundary == want {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Split(a, b) => {
                        stack.push((b, pos));
                        pc = a;
                    }
                    Inst::Jmp(a) => pc = a,
                    Inst::Match => return true,
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        compile(p).unwrap().is_match(s)
    }

    #[test]
    fn basics() {
        assert!(m("abc", "xxabcxx"));
        assert!(!m("^abc", "xxabc"));
        assert!(m("^abc$", "abc"));
        assert!(m("a|b", "zzb"));
        assert!(m("gr(a|e)y", "grey"));
        assert!(m("gr(a|e)y", "gray"));
        assert!(m("a.c", "abc"));
        assert!(m("[a-c]+", "xxbcx"));
        assert!(m("[^a-c]", "zzz"));
        assert!(m("^.*\\.rs$", "main.rs"));
        assert!(m("\\bfoo\\b", "a foo b"));
        assert!(!m("\\bfoo\\b", "afoob"));
        assert!(m("colou?r", "color"));
        assert!(m("a{2,3}", "caaab"));
        assert!(!m("^a{4}$", "aaa"));
        assert!(m("^a{4}$", "aaaa"));
        assert!(m("^.*\\.(zip|7z)$", "C:\\a\\b.7z"));
    }
}
