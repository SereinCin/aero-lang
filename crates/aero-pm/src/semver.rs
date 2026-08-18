//! SemVer 2.0.0 语义化版本模型与版本需求（范围）匹配。
//!
//! 手写、零依赖实现，保持工具链离线自包含：
//! - [`Version`]：`major.minor.patch[-pre][+build]`，按 SemVer 优先级比较。
//! - [`Requirement`]：版本需求，支持 `^`/`~`/裸版本（ caret 语义）、通配符 `*`/`X.Y`，
//!   以及 `=`/`>`/`>=`/`<`/`<=` 比较符与逗号/空格组合。

use std::cmp::Ordering;
use std::fmt;

/// 预发布标识符中的一个元素。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreIdent {
    Num(u64),
    Alpha(String),
}

impl PreIdent {
    /// SemVer 优先级：数字 < 字母；同为数字按数值比较，同为字母按字典序比较。
    fn precedence(&self, other: &PreIdent) -> Ordering {
        match (self, other) {
            (PreIdent::Num(a), PreIdent::Num(b)) => a.cmp(b),
            (PreIdent::Alpha(a), PreIdent::Alpha(b)) => a.cmp(b),
            (PreIdent::Num(_), PreIdent::Alpha(_)) => Ordering::Less,
            (PreIdent::Alpha(_), PreIdent::Num(_)) => Ordering::Greater,
        }
    }
}

/// 一个完整版本号 `major.minor.patch[-pre]`（构建元数据不参与比较，解析时丢弃）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// 预发布标识符；空 = 正式版。
    pub pre: Vec<PreIdent>,
}

impl Default for Version {
    fn default() -> Self {
        Version { major: 0, minor: 0, patch: 0, pre: Vec::new() }
    }
}

impl Version {
    /// 解析严格 `major.minor.patch`，可选 `-pre` 和 `+build`（`+build` 被忽略）。
    pub fn parse(s: &str) -> Option<Version> {
        // 丢弃 +build 部分
        let s = s.split('+').next().unwrap_or(s);
        let mut it = s.split('-');
        let num = it.next()?;
        let mut parts = num.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        let pre = match it.next() {
            Some(p) if !p.is_empty() => {
                let mut idents = Vec::new();
                for id in p.split('.') {
                    let is_num = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
                    if is_num {
                        idents.push(PreIdent::Num(id.parse().ok()?));
                    } else {
                        idents.push(PreIdent::Alpha(id.to_string()));
                    }
                }
                idents
            }
            _ => Vec::new(),
        };
        Some(Version { major, minor, patch, pre })
    }

    /// 带预发布版本：`X.Y.Z-pre`。
    fn has_pre(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> Ordering {
        let major = self.major.cmp(&other.major);
        if major != Ordering::Equal {
            return major;
        }
        let minor = self.minor.cmp(&other.minor);
        if minor != Ordering::Equal {
            return minor;
        }
        let patch = self.patch.cmp(&other.patch);
        if patch != Ordering::Equal {
            return patch;
        }
        // 无预发布 > 有预发布
        match (self.has_pre(), other.has_pre()) {
            (false, false) => Ordering::Equal,
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (true, true) => {
                for (a, b) in self.pre.iter().zip(other.pre.iter()) {
                    let c = a.precedence(b);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                self.pre.len().cmp(&other.pre.len())
            }
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-")?;
            for (i, id) in self.pre.iter().enumerate() {
                if i > 0 {
                    write!(f, ".")?;
                }
                match id {
                    PreIdent::Num(n) => write!(f, "{n}")?,
                    PreIdent::Alpha(a) => write!(f, "{a}")?,
                }
            }
        }
        Ok(())
    }
}

/// 比较操作符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Eq,
    Gt,
    GtEq,
    Lt,
    LtEq,
}

#[derive(Debug, Clone)]
pub(crate) struct Bound {
    op: Op,
    ver: Version,
}

/// 版本需求（一个或多个比较符的合取）。空集合 = 匹配任意版本（`*`）。
#[derive(Debug, Clone)]
pub struct Requirement {
    pub(crate) bounds: Vec<Bound>,
    raw: String,
}

impl Requirement {
    /// 解析一个版本需求字符串。
    pub fn parse(s: &str) -> Option<Requirement> {
        let raw = s.trim().to_string();
        if raw.is_empty() {
            return Some(Requirement { bounds: Vec::new(), raw: String::new() });
        }
        let mut bounds = Vec::new();
        // 按逗号分割成多个片段，每段再按空白分割。
        for seg in raw.split(',') {
            for tok in seg.split_whitespace() {
                let tok = tok.trim();
                if tok.is_empty() {
                    continue;
                }
                let (op, v) = parse_bound(tok)?;
                match op {
                    OwnedOp::Any => return Some(Requirement { bounds: Vec::new(), raw }),
                    other => bounds.extend(other.expand(&v)?),
                }
            }
        }
        Some(Requirement { bounds, raw })
    }

    /// 判断版本是否满足需求。预发布版本默认不匹配，除非需求显式含 `-pre` 片段。
    pub fn matches(&self, v: &Version) -> bool {
        if v.has_pre() && !self.raw.contains('-') {
            return false;
        }
        self.bounds.iter().all(|b| match b.op {
            Op::Eq => v == &b.ver,
            Op::Gt => v > &b.ver,
            Op::GtEq => v >= &b.ver,
            Op::Lt => v < &b.ver,
            Op::LtEq => v <= &b.ver,
        })
    }

    /// 从候选中选出匹配要求且版本最高的那个。
    pub fn select_highest<'a>(&self, candidates: &'a [Version]) -> Option<&'a Version> {
        candidates.iter().filter(|v| self.matches(v)).max()
    }

    /// 原始需求文本。
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.raw.is_empty() {
            write!(f, "*")
        } else {
            write!(f, "{}", self.raw)
        }
    }
}

/// 解析单个 token（操作符 + 解析出的组件）。
pub(crate) struct Parsed {
    major: u64,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Vec<PreIdent>,
}

impl Parsed {
    /// 补位为一个完整版本（缺省分量填 0，保留 pre）。
    fn full(&self) -> Version {
        Version {
            major: self.major,
            minor: self.minor.unwrap_or(0),
            patch: self.patch.unwrap_or(0),
            pre: self.pre.clone(),
        }
    }
}

/// 解析单个 token 前的“原始操作符”状态。
pub(crate) enum OwnedOp {
    Any,
    Caret,
    Tilde,
    /// 裸版本（裸部分按 caret）；`Wc(true)` = 显式通配符。
    Wc(bool),
    Cmp(Op),
}

/// 解析一个需求 token：返回（操作符，解析出的组件）。
fn parse_bound(tok: &str) -> Option<(OwnedOp, Parsed)> {
    if tok == "*" {
        return Some((
            OwnedOp::Any,
            Parsed { major: 0, minor: None, patch: None, pre: Vec::new() },
        ));
    }
    let (op, rest) = if let Some(r) = tok.strip_prefix('^') {
        (OwnedOp::Caret, r)
    } else if let Some(r) = tok.strip_prefix('~') {
        (OwnedOp::Tilde, r)
    } else if let Some(r) = tok.strip_prefix(">=") {
        (OwnedOp::Cmp(Op::GtEq), r)
    } else if let Some(r) = tok.strip_prefix("<=") {
        (OwnedOp::Cmp(Op::LtEq), r)
    } else if let Some(r) = tok.strip_prefix('=') {
        (OwnedOp::Cmp(Op::Eq), r)
    } else if let Some(r) = tok.strip_prefix('>') {
        (OwnedOp::Cmp(Op::Gt), r)
    } else if let Some(r) = tok.strip_prefix('<') {
        (OwnedOp::Cmp(Op::Lt), r)
    } else {
        // 裸版本（仅当以数字开头）
        if tok.starts_with(|c: char| c.is_ascii_digit()) {
            (OwnedOp::Wc(tok.contains('*')), tok)
        } else {
            return None;
        }
    };
    let parsed = parse_components(rest)?;
    Some((op, parsed))
}

/// 解析 `major[.minor[.patch]][-pre]`（缺省分量置 None，`*` 视作缺失）。
fn parse_components(rest: &str) -> Option<Parsed> {
    let rest = rest.split('+').next().unwrap_or(rest);
    let (num, pre_str) = match rest.split_once('-') {
        Some((n, p)) => (n, Some(p)),
        None => (rest, None),
    };
    let mut parts = num.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor = match parts.next() {
        Some(p) if !p.is_empty() && p != "*" => Some(p.parse().ok()?),
        _ => None,
    };
    let patch = match parts.next() {
        Some(p) if !p.is_empty() && p != "*" => Some(p.parse().ok()?),
        _ => None,
    };
    let pre = match pre_str {
        Some(p) if !p.is_empty() => parse_pre(p)?,
        _ => Vec::new(),
    };
    Some(Parsed {
        major,
        minor,
        patch,
        pre,
    })
}

/// 解析预发布标识符（数字/字母混排）。
fn parse_pre(p: &str) -> Option<Vec<PreIdent>> {
    let mut idents = Vec::new();
    for id in p.split('.') {
        if id.is_empty() {
            return None;
        }
        let is_num = id.bytes().all(|b| b.is_ascii_digit());
        if is_num {
            idents.push(PreIdent::Num(id.parse().ok()?));
        } else {
            idents.push(PreIdent::Alpha(id.to_string()));
        }
    }
    Some(idents)
}

impl OwnedOp {
    /// 展开为比较符集合。
    pub(crate) fn expand(&self, p: &Parsed) -> Option<Vec<Bound>> {
        let (major, ominor, opatch) = (p.major, p.minor, p.patch);
        let minor = ominor.unwrap_or(0);
        let patch = opatch.unwrap_or(0);
        let base = |m: u64, n: u64, pa: u64| Version { major: m, minor: n, patch: pa, pre: Vec::new() };
        match self {
            OwnedOp::Any => Some(Vec::new()),
            OwnedOp::Cmp(op) => Some(vec![Bound { op: *op, ver: p.full() }]),
            OwnedOp::Caret | OwnedOp::Wc(false) => {
                // bare 版本或 `^`：compatible range，由首个非零分量决定“ring”
                let lower = base(major, minor, patch);
                // 若需求显式带 pre，下界也要带上 pre（否则预发布永远匹配不上）
                let lower = if p.pre.is_empty() { lower } else { p.full() };
                let upper = if major > 0 {
                    base(major, 0, 0).bump_major()
                } else if ominor.is_some() && minor > 0 {
                    base(0, minor, 0).bump_minor()
                } else if opatch.is_some() {
                    base(0, 0, patch).bump_patch()
                } else {
                    base(0, 1, 0) // ^0
                };
                Some(ring(lower, upper))
            }
            OwnedOp::Tilde => {
                let lower = base(major, minor, patch);
                let lower = if p.pre.is_empty() { lower } else { p.full() };
                // ~1.2.3 -> <1.3.0; ~1.2 -> <1.3.0; ~1 -> <2.0.0
                let upper = if ominor.is_some() {
                    base(major, minor, 0).bump_minor()
                } else {
                    base(major, 0, 0).bump_major()
                };
                Some(ring(lower, upper))
            }
            OwnedOp::Wc(true) => {
                // 显式通配符：只要 minor 出现（`X.Y.*`/`X.Y.Z.*`）即闭 minor ring；
                // 否则仅 major（`X.*`）闭 major ring。确保 `1.2.*` 不匹配 `1.3.0`。
                if ominor.is_some() {
                    let lower = base(major, minor, 0);
                    let upper = base(major, minor, 0).bump_minor();
                    Some(ring(lower, upper))
                } else {
                    let lower = base(major, 0, 0);
                    let upper = base(major, 0, 0).bump_major();
                    Some(ring(lower, upper))
                }
            }
        }
    }
}

fn ring(lo: Version, hi: Version) -> Vec<Bound> {
    vec![
        Bound { op: Op::GtEq, ver: lo },
        Bound { op: Op::Lt, ver: hi },
    ]
}

impl Version {
    fn bump_major(&self) -> Version {
        Version { major: self.major + 1, minor: 0, patch: 0, pre: Vec::new() }
    }
    fn bump_minor(&self) -> Version {
        Version { major: self.major, minor: self.minor + 1, patch: 0, pre: Vec::new() }
    }
    fn bump_patch(&self) -> Version {
        Version { major: self.major, minor: self.minor, patch: self.patch + 1, pre: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).expect("valid version")
    }

    #[test]
    fn parses_full_version() {
        let x = v("1.2.3");
        assert_eq!((x.major, x.minor, x.patch), (1, 2, 3));
        assert!(x.pre.is_empty());
        let pre = v("1.2.3-alpha.1");
        assert_eq!(pre.pre, vec![PreIdent::Alpha("alpha".into()), PreIdent::Num(1)]);
        // build 元数据被忽略
        let b = v("1.2.3+build.5");
        assert_eq!(b, v("1.2.3"));
    }

    #[test]
    fn version_ordering() {
        assert!(v("1.2.3") < v("1.2.4"));
        assert!(v("1.2.0") < v("1.3.0"));
        assert!(v("1.9.0") < v("2.0.0"));
        assert!(v("0.2.3") < v("0.3.0"));
        assert!(v("1.0.0") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.1") < v("1.0.0-rc.2"));
        assert!(v("1.0.0-alpha") < v("1.0.0-rc"));
        // 数字 < 字母
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
    }

    #[test]
    fn caret_semantics() {
        let r = Requirement::parse("^1.2.3").unwrap();
        assert!(r.matches(&v("1.2.3")));
        assert!(r.matches(&v("1.9.9")));
        assert!(!r.matches(&v("2.0.0")));
        assert!(!r.matches(&v("1.2.2")));
        let r0 = Requirement::parse("^0.2.3").unwrap();
        assert!(r0.matches(&v("0.2.3")));
        assert!(r0.matches(&v("0.2.9")));
        assert!(!r0.matches(&v("0.3.0")));
        let r00 = Requirement::parse("^0.0.3").unwrap();
        assert!(r00.matches(&v("0.0.3")));
        assert!(!r00.matches(&v("0.0.4")));
        // 裸版本同 caret
        let bare = Requirement::parse("1.2").unwrap();
        assert!(bare.matches(&v("1.2.0")));
        assert!(bare.matches(&v("1.9.1")));
        assert!(!bare.matches(&v("2.0.0")));
    }

    #[test]
    fn tilde_and_wildcard() {
        let tilde = Requirement::parse("~1.2.3").unwrap();
        assert!(tilde.matches(&v("1.2.3")));
        assert!(!tilde.matches(&v("1.3.0")));
        let wild = Requirement::parse("1.2.*").unwrap();
        assert!(wild.matches(&v("1.2.9")));
        assert!(!wild.matches(&v("1.3.0")));
        let any = Requirement::parse("*").unwrap();
        assert!(any.matches(&v("0.0.0")));
        assert!(any.matches(&v("9.9.9")));
    }

    #[test]
    fn comparators_and_ranges() {
        let ge = Requirement::parse(">=1.0").unwrap();
        assert!(ge.matches(&v("1.0.0")));
        assert!(ge.matches(&v("5.0.0")));
        assert!(!ge.matches(&v("0.9.9")));
        let range = Requirement::parse(">=1.0, <2.0").unwrap();
        assert!(range.matches(&v("1.5.0")));
        assert!(!range.matches(&v("2.0.0")));
        assert!(!range.matches(&v("0.9.0")));
        let exact = Requirement::parse("=1.2.3").unwrap();
        assert!(exact.matches(&v("1.2.3")));
        assert!(!exact.matches(&v("1.2.4")));
    }

    #[test]
    fn prerelease_guard() {
        let req = Requirement::parse(">=1.0.0").unwrap();
        assert!(!req.matches(&v("2.0.0-beta")));
        // 需求显式含 pre 才匹配
        let pre = Requirement::parse(">=2.0.0-beta").unwrap();
        assert!(pre.matches(&v("2.0.0-beta")));
    }

    #[test]
    fn select_highest_matching() {
        let cands = [
            v("0.9.0"),
            v("1.2.0"),
            v("1.4.5"),
            v("2.0.0"),
        ];
        let req = Requirement::parse("^1.0.0").unwrap();
        assert_eq!(*req.select_highest(&cands).unwrap(), v("1.4.5"));
    }

    #[test]
    fn invalid_rejected() {
        assert!(Requirement::parse("abc").is_none());
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("a.b.c").is_none());
        assert!(Version::parse("").is_none());
    }
}