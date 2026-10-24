//! Everything's search language: tokenizer, parser and the compiled filter
//! tree.
//!
//! Grammar (highest precedence first):
//!
//! ```text
//! term    := "!" term | "(" expr ")" | macro | function | word
//! and     := term (term)*                     -- whitespace is AND
//! or      := and ("|" and)*
//! ```
//!
//! Supported modifiers: `case:` `nocase:` `regex:` `noregex:` `ww:` `noww:`
//! `wildcards:` `nowildcards:` `path:` `nopath:` `ascii:` `utf8:` `diacritics:`
//! `file:` `folder:` `startwith:` `endwith:` `wholefilename:`
//!
//! Supported functions: `ext:` `size:` `dm:` `dc:` `da:` `rc:` `attrib:`
//! `len:` `parent:` `infolder:` `path:` `parents:` `depth:` `root:` `frn:`
//! `dupe:` `sizedupe:` `dmdupe:` `dcdupe:` `dadupe:` `empty:` `count:` `type:`
//! plus the media/archive macros.

use std::sync::Arc;

use crate::date::{self, LocalClock, ParsedDate};
use crate::pattern::{MatchOptions, NameMatcher, Target};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateField {
    Modified,
    Created,
    Accessed,
    RecentChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DupeKind {
    Name,
    Size,
    DateModified,
    DateCreated,
    DateAccessed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeClass {
    Audio,
    Video,
    Picture,
    Document,
    Executable,
    Compressed,
    Folder,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp<T> {
    Eq(T),
    Lt(T),
    Le(T),
    Gt(T),
    Ge(T),
    Range(T, T),
}

impl<T: PartialOrd + Copy> Cmp<T> {
    #[inline]
    pub fn test(&self, v: T) -> bool {
        match *self {
            Cmp::Eq(x) => v == x,
            Cmp::Lt(x) => v < x,
            Cmp::Le(x) => v <= x,
            Cmp::Gt(x) => v > x,
            Cmp::Ge(x) => v >= x,
            Cmp::Range(lo, hi) => v >= lo && v <= hi,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DateRange {
    Unknown,
    Between(i64, i64),
}

#[derive(Debug, Clone)]
pub enum Leaf {
    Name(NameMatcher),
    Ext(Arc<Vec<String>>),
    Size(Cmp<u64>),
    Date(DateField, DateRange),
    Attrib { must: u32, must_not: u32 },
    NameLen(Cmp<u32>),
    ParentPath { prefix: String, direct: bool },
    Depth(Cmp<u32>),
    Root,
    Frn(u64),
    EmptyDir,
    Type(TypeClass),
}

#[derive(Debug, Clone)]
pub enum Node {
    True,
    False,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Leaf),
}

#[derive(Debug, Clone)]
pub struct Query {
    pub raw: String,
    pub root: Node,
    pub limit: Option<usize>,
    pub dupe: Option<DupeKind>,
    pub only: Option<bool>,
    pub warnings: Vec<String>,
    pub is_empty: bool,
    /// Terms that need no metadata; lets the engine pick the tight fast path.
    pub name_terms: usize,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            raw: String::new(),
            root: Node::True,
            limit: None,
            dupe: None,
            only: None,
            warnings: Vec::new(),
            is_empty: true,
            name_terms: 0,
        }
    }
}

impl Query {
    pub fn parse(input: &str, clock: &LocalClock) -> Query {
        let raw = input.trim().to_string();
        if raw.is_empty() {
            return Query {
                raw,
                ..Default::default()
            };
        }
        let (toks, mut warnings) = tokenize(&raw);
        let flags = prescan(&toks);
        let only = flags.only;
        let mut p = Parser {
            toks: &toks,
            pos: 0,
            flags,
            warnings: &mut warnings,
            clock,
            limit: None,
            dupe: None,
            only,
            name_terms: 0,
        };
        let root = p.parse_or();
        let limit = p.limit;
        let dupe = p.dupe;
        let only = p.only;
        let name_terms = p.name_terms;
        drop(p);
        let mut q = Query {
            raw,
            root,
            limit,
            dupe,
            only,
            warnings,
            is_empty: false,
            name_terms,
        };
        q.flatten();
        q
    }

    /// Collapse singleton nodes so the evaluator has less to walk.
    fn flatten(&mut self) {
        fn walk(n: Node) -> Node {
            match n {
                Node::And(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for it in items.into_iter().map(walk) {
                        match it {
                            Node::True => {}
                            Node::And(inner) => out.extend(inner),
                            other => out.push(other),
                        }
                    }
                    match out.len() {
                        0 => Node::True,
                        1 => out.pop().unwrap(),
                        _ => {
                            out.sort_by_key(cost);
                            Node::And(out)
                        }
                    }
                }
                Node::Or(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for it in items.into_iter().map(walk) {
                        match it {
                            Node::False => {}
                            Node::Or(inner) => out.extend(inner),
                            other => out.push(other),
                        }
                    }
                    match out.len() {
                        0 => Node::False,
                        1 => out.pop().unwrap(),
                        _ => Node::Or(out),
                    }
                }
                Node::Not(b) => {
                    let inner = walk(*b);
                    match inner {
                        Node::True => Node::False,
                        Node::False => Node::True,
                        other => Node::Not(Box::new(other)),
                    }
                }
                other => other,
            }
        }
        self.root = walk(std::mem::replace(&mut self.root, Node::True));
    }

    pub fn is_match_all(&self) -> bool {
        matches!(self.root, Node::True)
    }
}

/// Cheapest / most selective predicates run first inside an AND.
fn cost(n: &Node) -> i32 {
    match n {
        Node::True | Node::False => 0,
        Node::Not(b) => cost(b) + 1,
        Node::Or(_) => 50,
        Node::Leaf(l) => match l {
            Leaf::Name(m) => 10 + (200usize.saturating_sub(m.selectivity())) as i32,
            Leaf::Ext(_) => 20,
            Leaf::Attrib { .. } => 20,
            Leaf::NameLen(_) => 25,
            Leaf::Size(_) => 30,
            Leaf::Depth(_) | Leaf::Root => 30,
            Leaf::Frn(_) => 15,
            Leaf::ParentPath { .. } => 40,
            Leaf::Date(..) => 60,
            Leaf::EmptyDir => 70,
            Leaf::Type(_) => 20,
        },
        Node::And(items) => items.iter().map(cost).min().unwrap_or(0),
    }
}

// ---------------------------------------------------------------- tokenizer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Quoted(String),
    LParen,
    RParen,
    Or,
    Not,
}

fn tokenize(input: &str) -> (Vec<Tok>, Vec<String>) {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                out.push(Tok::LParen);
                i += 1;
                continue;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
                continue;
            }
            '|' => {
                out.push(Tok::Or);
                i += 1;
                continue;
            }
            '!' => {
                out.push(Tok::Not);
                i += 1;
                continue;
            }
            _ => {}
        }
        let mut buf = String::new();
        let mut was_quoted = false;
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() || c == '|' || c == '(' || c == ')' {
                break;
            }
            if c == '"' || c == '\'' {
                was_quoted = true;
                let quote = c;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == quote {
                        closed = true;
                        i += 1;
                        break;
                    }
                    buf.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    warnings.push("unterminated quote".to_string());
                }
                continue;
            }
            buf.push(c);
            i += 1;
        }
        if buf.is_empty() {
            continue;
        }
        out.push(if was_quoted {
            Tok::Quoted(buf)
        } else {
            Tok::Word(buf)
        });
    }
    (out, warnings)
}

#[derive(Debug, Clone, Copy)]
struct Flags {
    case: bool,
    regex: bool,
    wildcards: bool,
    whole_word: bool,
    path: bool,
    only: Option<bool>,
}

impl Default for Flags {
    fn default() -> Self {
        Self {
            case: false,
            regex: false,
            // Everything enables * and ? wildcards unless they are turned off.
            wildcards: true,
            whole_word: false,
            path: false,
            only: None,
        }
    }
}

fn prescan(toks: &[Tok]) -> Flags {
    let mut f = Flags::default();
    for t in toks {
        let Tok::Word(w) = t else { continue };
        let Some(name) = w.strip_suffix(':') else {
            continue;
        };
        apply_bare_modifier(&mut f, &name.to_ascii_lowercase());
    }
    f
}

fn apply_bare_modifier(f: &mut Flags, name: &str) -> bool {
    match name {
        "case" => f.case = true,
        "nocase" => f.case = false,
        "regex" => f.regex = true,
        "noregex" => f.regex = false,
        "wildcards" => f.wildcards = true,
        "nowildcards" | "nowildcard" => f.wildcards = false,
        "ww" | "wholeword" => f.whole_word = true,
        "noww" | "nowholeword" => f.whole_word = false,
        "path" | "matchpath" => f.path = true,
        "nopath" | "nomatchpath" => f.path = false,
        "file" => f.only = Some(false),
        "folder" => f.only = Some(true),
        _ => return false,
    }
    true
}

// ------------------------------------------------------------------- parser

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    flags: Flags,
    warnings: &'a mut Vec<String>,
    clock: &'a LocalClock,
    limit: Option<usize>,
    dupe: Option<DupeKind>,
    only: Option<bool>,
    name_terms: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_or(&mut self) -> Node {
        let mut parts = vec![self.parse_and()];
        while matches!(self.peek(), Some(Tok::Or)) {
            self.pos += 1;
            parts.push(self.parse_and());
        }
        if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Node::Or(parts)
        }
    }

    fn parse_and(&mut self) -> Node {
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                None | Some(Tok::Or) | Some(Tok::RParen) => break,
                _ => {
                    let n = self.parse_unary();
                    if !matches!(n, Node::True) {
                        parts.push(n);
                    }
                }
            }
        }
        match parts.len() {
            0 => Node::True,
            1 => parts.pop().unwrap(),
            _ => Node::And(parts),
        }
    }

    fn parse_unary(&mut self) -> Node {
        if matches!(self.peek(), Some(Tok::Not)) {
            self.pos += 1;
            return Node::Not(Box::new(self.parse_unary()));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Node {
        match self.next() {
            Some(Tok::LParen) => {
                let n = self.parse_or();
                if matches!(self.peek(), Some(Tok::RParen)) {
                    self.pos += 1;
                } else {
                    self.warnings.push("missing ')'".into());
                }
                n
            }
            Some(Tok::RParen) => {
                self.warnings.push("unexpected ')'".into());
                Node::True
            }
            Some(Tok::Word(w)) => self.word(&w),
            Some(Tok::Quoted(w)) => self.literal(&w),
            Some(Tok::Or) | Some(Tok::Not) | None => Node::True,
        }
    }

    fn literal(&mut self, text: &str) -> Node {
        if text.is_empty() {
            return Node::True;
        }
        let opts = MatchOptions {
            case: self.flags.case,
            whole_word: self.flags.whole_word,
            wildcards: self.flags.wildcards,
            regex: self.flags.regex,
            ..Default::default()
        };
        let target = if self.flags.path {
            Target::Path
        } else {
            Target::Name
        };
        match NameMatcher::new(text, target, opts) {
            Ok(m) => {
                self.name_terms += 1;
                Node::Leaf(Leaf::Name(m))
            }
            Err(e) => {
                self.warnings.push(e);
                Node::True
            }
        }
    }

    fn word(&mut self, w: &str) -> Node {
        if w.is_empty() {
            return Node::True;
        }
        if let Some((prefix, value)) = w.split_once(':') {
            let name = prefix.to_ascii_lowercase();
            if name.is_empty() {
                return self.literal(w);
            }
            if value.is_empty() {
                // Bare modifier: `case:`, `regex:`, ...
                if apply_bare_modifier(&mut self.flags, &name) {
                    if matches!(name.as_str(), "file" | "folder") {
                        self.only = self.flags.only;
                    }
                    return Node::True;
                }
                // A trailing colon on an unknown word is a literal search.
                return self.literal(w);
            }
            // A modifier with an attached value scopes the flag to this term.
            if is_scoped_modifier(&name) {
                let saved = self.flags;
                if apply_bare_modifier(&mut self.flags, &name) {
                    if matches!(name.as_str(), "file" | "folder") {
                        self.only = self.flags.only;
                    }
                    let n = self.word_value(value);
                    self.flags = saved;
                    return n;
                }
            }
            if let Some(n) = self.function(&name, value) {
                return n;
            }
        }
        self.word_value(w)
    }

    fn word_value(&mut self, text: &str) -> Node {
        let opts = MatchOptions {
            case: self.flags.case,
            whole_word: self.flags.whole_word,
            wildcards: self.flags.wildcards,
            regex: self.flags.regex,
            ..Default::default()
        };
        let target = if self.flags.path {
            Target::Path
        } else {
            Target::Name
        };
        match NameMatcher::new(text, target, opts) {
            Ok(m) => {
                self.name_terms += 1;
                Node::Leaf(Leaf::Name(m))
            }
            Err(e) => {
                self.warnings.push(e);
                Node::True
            }
        }
    }

    fn named(&mut self, kind: crate::pattern::Kind, text: &str) -> Node {
        let opts = MatchOptions {
            case: self.flags.case,
            whole_word: self.flags.whole_word,
            wildcards: true,
            start_with: kind == crate::pattern::Kind::Prefix,
            end_with: kind == crate::pattern::Kind::Suffix,
            whole_filename: kind == crate::pattern::Kind::Exact
                || kind == crate::pattern::Kind::Glob,
            ..Default::default()
        };
        let target = if self.flags.path {
            Target::Path
        } else {
            Target::Name
        };
        match NameMatcher::new(text, target, opts) {
            Ok(m) => {
                self.name_terms += 1;
                Node::Leaf(Leaf::Name(m))
            }
            Err(e) => {
                self.warnings.push(e);
                Node::True
            }
        }
    }

    fn function(&mut self, name: &str, value: &str) -> Option<Node> {
        let v = value.trim();
        match name {
            "ext" => {
                let list: Vec<String> = v
                    .split([';', ','])
                    .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                if list.is_empty() {
                    None
                } else {
                    Some(Node::Leaf(Leaf::Ext(Arc::new(list))))
                }
            }
            "size" => parse_size(v).map(|c| Node::Leaf(Leaf::Size(c))),
            "dm" | "datemodified" => self.date_leaf(DateField::Modified, v),
            "dc" | "datecreated" => self.date_leaf(DateField::Created, v),
            "da" | "dateaccessed" => self.date_leaf(DateField::Accessed, v),
            "rc" | "recentchange" | "dr" | "daterun" => {
                self.date_leaf(DateField::RecentChange, v)
            }
            "attrib" | "attributes" => {
                let (must, must_not) = parse_attrib(v);
                if must == 0 && must_not == 0 {
                    self.warnings.push(format!("unknown attributes '{v}'"));
                    None
                } else {
                    Some(Node::Leaf(Leaf::Attrib { must, must_not }))
                }
            }
            "len" | "length" => parse_u32_cmp(v).map(|c| Node::Leaf(Leaf::NameLen(c))),
            "parent" => Some(Node::Leaf(Leaf::ParentPath {
                prefix: normalize_path(v),
                direct: true,
            })),
            "infolder" => Some(Node::Leaf(Leaf::ParentPath {
                prefix: normalize_path(v),
                direct: false,
            })),
            "path" => Some(self.named(crate::pattern::Kind::Contains, v)),
            "parents" | "depth" => parse_u32_cmp(v).map(|c| Node::Leaf(Leaf::Depth(c))),
            "root" => Some(Node::Leaf(Leaf::Root)),
            "frn" => v
                .trim_start_matches("0x")
                .parse::<u64>()
                .ok()
                .map(|n| Node::Leaf(Leaf::Frn(n))),
            "dupe" => {
                self.dupe = Some(DupeKind::Name);
                Some(Node::True)
            }
            "sizedupe" | "dupesize" => {
                self.dupe = Some(DupeKind::Size);
                Some(Node::True)
            }
            "dmdupe" | "dupedm" | "datemodifieddupe" => {
                self.dupe = Some(DupeKind::DateModified);
                Some(Node::True)
            }
            "dcdupe" | "dupedc" | "datecreateddupe" => {
                self.dupe = Some(DupeKind::DateCreated);
                Some(Node::True)
            }
            "dadupe" | "dupeda" | "dateaccesseddupe" => {
                self.dupe = Some(DupeKind::DateAccessed);
                Some(Node::True)
            }
            "empty" => Some(Node::Leaf(Leaf::EmptyDir)),
            "count" => {
                self.limit = v.parse::<usize>().ok();
                Some(Node::True)
            }
            "type" => parse_type(v).map(|t| Node::Leaf(Leaf::Type(t))),
            "startwith" | "startswith" => Some(self.named(crate::pattern::Kind::Prefix, v)),
            "endwith" | "endswith" => Some(self.named(crate::pattern::Kind::Suffix, v)),
            "wholefilename" => Some(self.named(crate::pattern::Kind::Exact, v)),
            "audio" => Some(ext_list(AUDIO_EXT)),
            "video" => Some(ext_list(VIDEO_EXT)),
            "doc" | "document" => Some(ext_list(DOC_EXT)),
            "pic" | "picture" | "image" => Some(ext_list(PICTURE_EXT)),
            "exe" | "executable" => Some(ext_list(EXEC_EXT)),
            "zip" | "compressed" | "archive" => Some(ext_list(ZIP_EXT)),
            "child" | "childcount" | "childfilecount" | "childfoldercount" | "namepartdupe"
            | "filelist" | "filelistfilename" | "fsi" | "shell" | "runcount" | "album"
            | "artist" | "title" | "track" | "genre" | "comment" | "width" | "height"
            | "dimensions" | "orientation" | "bitdepth" | "content" | "ansicontent"
            | "utf8content" | "utf16content" | "utf16becontent" | "unicodecontent"
            | "wcharcontent" | "utf8" | "ascii" | "diacritics" | "nodiacritics" => {
                self.warnings
                    .push(format!("'{name}:' is not implemented by everything-rs"));
                Some(Node::True)
            }
            _ => None,
        }
    }

    fn date_leaf(&mut self, field: DateField, v: &str) -> Option<Node> {
        let (op, rest) = split_op(v);
        if let Some((a, b)) = split_range(rest) {
            let da = date::parse_date(a, self.clock);
            let db = date::parse_date(b, self.clock);
            match (da, db) {
                (ParsedDate::Range(s, _), ParsedDate::Range(_, e)) => {
                    return Some(Node::Leaf(Leaf::Date(field, DateRange::Between(s, e))));
                }
                _ => return None,
            }
        }
        match date::parse_date(rest, self.clock) {
            ParsedDate::Unknown => Some(Node::Leaf(Leaf::Date(field, DateRange::Unknown))),
            ParsedDate::Range(s, e) => {
                let range = match op {
                    Op::Eq => DateRange::Between(s, e - 1),
                    Op::Lt => DateRange::Between(i64::MIN, s - 1),
                    Op::Le => DateRange::Between(i64::MIN, e - 1),
                    Op::Gt => DateRange::Between(e, i64::MAX),
                    Op::Ge => DateRange::Between(s, i64::MAX),
                };
                Some(Node::Leaf(Leaf::Date(field, range)))
            }
            ParsedDate::Invalid => {
                self.warnings.push(format!("invalid date '{v}'"));
                None
            }
        }
    }
}

fn is_scoped_modifier(name: &str) -> bool {
    matches!(
        name,
        "case"
            | "nocase"
            | "regex"
            | "noregex"
            | "wildcards"
            | "nowildcards"
            | "ww"
            | "wholeword"
            | "noww"
            | "nowholeword"
            | "path"
            | "nopath"
            | "file"
            | "folder"
    )
}

fn ext_list(list: &'static [&'static str]) -> Node {
    Node::Leaf(Leaf::Ext(Arc::new(
        list.iter().map(|s| s.to_string()).collect(),
    )))
}

fn normalize_path(p: &str) -> String {
    let p = p.trim().trim_matches('"');
    let mut s = p.replace('/', "\\");
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    if s.len() == 2 && s.ends_with(':') {
        s.push('\\');
    }
    s
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

fn split_op(v: &str) -> (Op, &str) {
    let v = v.trim();
    if let Some(r) = v.strip_prefix("<=") {
        (Op::Le, r)
    } else if let Some(r) = v.strip_prefix(">=") {
        (Op::Ge, r)
    } else if let Some(r) = v.strip_prefix('<') {
        (Op::Lt, r)
    } else if let Some(r) = v.strip_prefix('>') {
        (Op::Gt, r)
    } else if let Some(r) = v.strip_prefix('=') {
        (Op::Eq, r)
    } else {
        (Op::Eq, v)
    }
}

fn split_range(v: &str) -> Option<(&str, &str)> {
    if let Some((a, b)) = v.split_once("..") {
        return Some((a.trim(), b.trim()));
    }
    // Fall back to "a-b" but do not split dates like 2024-05-03.
    if v.matches('-').count() == 1 {
        if let Some((a, b)) = v.split_once('-') {
            if !a.trim().is_empty() && !b.trim().is_empty() {
                return Some((a.trim(), b.trim()));
            }
        }
    }
    None
}

fn parse_u32_cmp(v: &str) -> Option<Cmp<u32>> {
    if let Some((a, b)) = split_range(v) {
        let a = a.parse::<u32>().ok()?;
        let b = b.parse::<u32>().ok()?;
        return Some(Cmp::Range(a, b));
    }
    let (op, rest) = split_op(v);
    let n = rest.trim().parse::<u32>().ok()?;
    Some(match op {
        Op::Eq => Cmp::Eq(n),
        Op::Lt => Cmp::Lt(n),
        Op::Le => Cmp::Le(n),
        Op::Gt => Cmp::Gt(n),
        Op::Ge => Cmp::Ge(n),
    })
}

pub fn parse_size(v: &str) -> Option<Cmp<u64>> {
    let v = v.trim().to_ascii_lowercase();
    let constant = match v.as_str() {
        "empty" => return Some(Cmp::Eq(0)),
        "tiny" => Some((1, 10 * 1024)),
        "small" => Some((10 * 1024 + 1, 100 * 1024)),
        "medium" => Some((100 * 1024 + 1, 1024 * 1024)),
        "large" => Some((1024 * 1024 + 1, 16 * 1024 * 1024)),
        "huge" => Some((16 * 1024 * 1024 + 1, 128 * 1024 * 1024)),
        "gigantic" => Some((128 * 1024 * 1024 + 1, u64::MAX)),
        "unknown" => Some((u64::MAX, u64::MAX)),        _ => None,
    };
    if let Some((a, b)) = constant {
        return Some(Cmp::Range(a, b));
    }
    if let Some((a, b)) = split_range(&v) {
        let a = parse_size_value(a)?;
        let b = parse_size_value(b)?;
        return Some(Cmp::Range(a, b));
    }
    let (op, rest) = split_op(&v);
    let n = parse_size_value(rest)?;
    Some(match op {
        Op::Eq => Cmp::Eq(n),
        Op::Lt => Cmp::Lt(n),
        Op::Le => Cmp::Le(n),
        Op::Gt => Cmp::Gt(n),
        Op::Ge => Cmp::Ge(n),
    })
}

fn parse_size_value(v: &str) -> Option<u64> {
    let v = v.trim();
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim() {
        "" | "b" | "bytes" | "byte" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = n * mult;
    if !bytes.is_finite() || bytes < 0.0 {
        return None;
    }
    Some(bytes as u64)
}

fn parse_attrib(v: &str) -> (u32, u32) {
    let mut must = 0u32;
    let mut must_not = 0u32;
    for part in v.split([';', ',', ' ']).filter(|p| !p.is_empty()) {
        let (neg, name) = match part.strip_prefix(['!', '-']) {
            Some(rest) => (true, rest),
            None => (false, part),
        };
        let bits = attrib_bits(name);
        if bits == 0 {
            continue;
        }
        if neg {
            must_not |= bits;
        } else {
            must |= bits;
        }
    }
    (must, must_not)
}

/// Accepts both `D`/`HD` style letter strings and full attribute names.
fn attrib_bits(name: &str) -> u32 {
    let letter = |c: char| -> u32 {
        match c.to_ascii_uppercase() {
            'A' => 0x20,
            'C' => 0x800,
            'D' => 0x10,
            'E' => 0x4000,
            'H' => 0x2,
            'I' => 0x2000,
            'L' => 0x400,
            'N' => 0x80,
            'O' => 0x1000,
            'P' => 0x200,
            'R' => 0x1,
            'S' => 0x4,
            'T' => 0x100,
            _ => 0,
        }
    };
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "archive" => 0x20,
        "compressed" => 0x800,
        "directory" | "folder" => 0x10,
        "encrypted" => 0x4000,
        "hidden" => 0x2,
        "notcontentindexed" | "notcontentindexedfile" => 0x2000,
        "reparsepoint" | "reparse" => 0x400,
        "normal" => 0x80,
        "offline" => 0x1000,
        "sparsefile" | "sparse" => 0x200,
        "readonly" => 0x1,
        "system" => 0x4,
        "temporary" => 0x100,
        _ => 0,
    };
    if named != 0 {
        return named;
    }
    name.chars()
        .map(letter)
        .fold(0u32, |acc, b| acc | b)
}

fn parse_type(v: &str) -> Option<TypeClass> {
    Some(match v.trim().to_ascii_lowercase().as_str() {
        "audio" | "music" => TypeClass::Audio,
        "video" | "movie" => TypeClass::Video,
        "pic" | "picture" | "image" => TypeClass::Picture,
        "doc" | "document" => TypeClass::Document,
        "exe" | "executable" | "program" => TypeClass::Executable,
        "zip" | "compressed" | "archive" => TypeClass::Compressed,
        "folder" | "directory" => TypeClass::Folder,
        "file" => TypeClass::File,
        _ => return None,
    })
}

pub fn type_ext_list(t: TypeClass) -> &'static [&'static str] {
    match t {
        TypeClass::Audio => AUDIO_EXT,
        TypeClass::Video => VIDEO_EXT,
        TypeClass::Picture => PICTURE_EXT,
        TypeClass::Document => DOC_EXT,
        TypeClass::Executable => EXEC_EXT,
        TypeClass::Compressed => ZIP_EXT,
        TypeClass::Folder | TypeClass::File => &[],
    }
}

// Extension groups lifted from the shipped Everything help resources.
pub const AUDIO_EXT: &[&str] = &[
    "aac", "ac3", "aif", "aifc", "aiff", "au", "cda", "dts", "fla", "flac", "it", "m1a", "m2a",
    "m3u", "m4a", "m4b", "m4p", "mid", "midi", "mka", "mod", "mp2", "mp3", "mpa", "ogg", "ra",
    "rmi", "snd", "spc", "umx", "voc", "wav", "wma", "xm",
];
pub const VIDEO_EXT: &[&str] = &[
    "3g2", "3gp", "3gp2", "3gpp", "amr", "amv", "asf", "avi", "bdmv", "bik", "d2v", "divx", "drc",
    "dsa", "dsm", "dss", "dsv", "evo", "f4v", "flc", "fli", "flic", "flv", "hdmov", "ifo", "ivf",
    "m1v", "m2p", "m2t", "m2ts", "m2v", "m4v", "mkv", "mp2v", "mp4", "mp4v", "mpe", "mpeg", "mpg",
    "mpls", "mpv2", "mpv4", "mov", "mts", "ogm", "ogv", "pss", "pva", "qt", "ram", "ratdvd", "rm",
    "rmm", "rmvb", "roq", "rpm", "smil", "smk", "swf", "tp", "tpr", "ts", "vob", "vp6", "webm",
    "wm", "wmp", "wmv",
];
pub const PICTURE_EXT: &[&str] = &[
    "ani", "bmp", "gif", "ico", "jpe", "jpeg", "jpg", "pcx", "png", "psd", "tga", "tif", "tiff",
    "webp", "wmf",
];
pub const DOC_EXT: &[&str] = &[
    "c", "chm", "cpp", "csv", "cxx", "doc", "docm", "docx", "dot", "dotm", "dotx", "h", "hpp",
    "htm", "html", "hxx", "ini", "java", "lua", "mht", "mhtml", "odt", "pdf", "potx", "potm",
    "ppam", "ppsm", "ppsx", "pps", "ppt", "pptm", "pptx", "rtf", "sldm", "sldx", "thmx", "txt",
    "vsd", "wpd", "wps", "wri", "xlam", "xls", "xlsb", "xlsm", "xlsx", "xltm", "xltx", "xml",
];
pub const EXEC_EXT: &[&str] = &["bat", "cmd", "exe", "msi", "msp", "scr"];
pub const ZIP_EXT: &[&str] = &[
    "7z", "ace", "arj", "bz2", "cab", "gz", "gzip", "jar", "r00", "r01", "r02", "r03", "r04",
    "r05", "r06", "r07", "r08", "r09", "r10", "rar", "tar", "tgz", "z", "zip",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> Query {
        Query::parse(s, &date::local_clock())
    }

    #[test]
    fn parses_plain() {
        let parsed = q("hello world");
        assert!(matches!(parsed.root, Node::And(ref v) if v.len() == 2));
    }

    #[test]
    fn parses_or_and_not() {
        let parsed = q("a|b !c");
        assert!(matches!(parsed.root, Node::Or(_)));
        let parsed = q("!(a b)");
        assert!(matches!(parsed.root, Node::Not(_)));
    }

    #[test]
    fn parses_functions() {
        let parsed = q("ext:rs;toml size:>1mb dm:today");
        match parsed.root {
            Node::And(items) => {
                assert!(matches!(items[0], Node::Leaf(Leaf::Ext(_))));
                assert!(matches!(items[1], Node::Leaf(Leaf::Size(_))));
                assert!(matches!(items[2], Node::Leaf(Leaf::Date(..))));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1kb"), Some(Cmp::Eq(1024)));
        assert!(matches!(parse_size(">2mb"), Some(Cmp::Gt(_))));
        assert_eq!(parse_size(">2mb"), Some(Cmp::Gt(2 * 1024 * 1024)));
        assert!(matches!(parse_size("empty"), Some(Cmp::Eq(0))));
    }

    #[test]
    fn quotes() {
        let parsed = q("\"hello world\"");
        assert!(matches!(parsed.root, Node::Leaf(Leaf::Name(_))));
    }
}
