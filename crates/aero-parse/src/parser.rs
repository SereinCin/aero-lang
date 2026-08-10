use aero_lex::token::{Token, TokenKind};

use crate::ast::{BinOp, CmpOp, Expr, LogicOp, Program, Stmt, TypeExpr, UnOp};
use crate::span::Span;

/// Unified representation of infix operators, so Pratt parsing can
/// distinguish the three AST node kinds.
enum InfixOp {
    Arith(BinOp),
    Cmp(CmpOp),
    Logic(LogicOp),
}

/// Syntax error with line/column position.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub msg: String,
    pub line: u32,
    pub col: u32,
}

/// Recursive-descent + Pratt (operator-precedence) parser.
pub struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    pub fn new(tokens: &'a [Token]) -> Self {
        Parser { tokens, pos: 0 }
    }

    /// Parse a whole program: `stmt*`
    pub fn parse_program(&mut self) -> Result<Program, ParseError> {
        let mut stmts = Vec::new();
        while !self.at_eof() {
            stmts.push(self.parse_stmt()?);
        }
        Ok(Program { stmts })
    }

    // ---------- statements ----------

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        match self.peek() {
            Some(t) if t.kind == TokenKind::Let => self.parse_let(),
            Some(t) if t.kind == TokenKind::Print => self.parse_print(),
            Some(t) if t.kind == TokenKind::If => self.parse_if(),
            Some(t) if t.kind == TokenKind::While => self.parse_while(),
            Some(t) if t.kind == TokenKind::Fn => self.parse_fn(),
            Some(t) if t.kind == TokenKind::Extern => self.parse_extern_fn(),
            Some(t) if t.kind == TokenKind::Return => self.parse_return(),
            _ => self.parse_expr_stmt(),
        }
    }

    /// `extern "gpu" fn ...` (GPU kernel, has a body) or `extern "C" fn ...;`
    /// (FFI declaration, no body).
    fn parse_extern_fn(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `extern`");
        // External model: `"gpu"` (kernel, has a body) or `"C"` (FFI, no body).
        // Both accept a string or a bare identifier.
        let model: &str = match self.peek() {
            Some(t) if matches!(&t.kind, TokenKind::Str(s) if s == "gpu") => {
                self.advance();
                "gpu"
            }
            Some(t) if matches!(&t.kind, TokenKind::Str(s) if s == "C") => {
                self.advance();
                "C"
            }
            Some(t) if matches!(t.kind, TokenKind::Gpu) => {
                self.advance();
                "gpu"
            }
            Some(t) if matches!(&t.kind, TokenKind::Ident(s) if s == "C") => {
                self.advance();
                "C"
            }
            other => {
                let (line, col) = match other {
                    Some(t) => (t.line, t.col),
                    None => (start.line, start.col),
                };
                return Err(ParseError {
                    msg: "after `extern` must come `\"gpu\"` or `\"C\"`".to_string(),
                    line,
                    col,
                });
            }
        };
        if !self.at(&TokenKind::Fn) {
            let tok = self
                .peek()
                .map(|t| (t.line, t.col))
                .unwrap_or_else(|| (start.line, start.col));
            return Err(ParseError {
                msg: format!("after extern \"{model}\" must come `fn`"),
                line: tok.0,
                col: tok.1,
            });
        }
        match model {
            "gpu" => self.parse_fn_with_gpu(true),
            _ => self.parse_extern_c_fn(&start),
        }
    }

    /// `extern "C" fn <name>(<params>) [-> <ret>];` — FFI external function
    /// declaration. Unlike a regular function it has no body and no generics;
    /// the symbol is resolved at link time.
    fn parse_extern_c_fn(&mut self, start: &Token) -> Result<Stmt, ParseError> {
        self.advance().expect("already checked for `fn`");
        let name = self.expect_ident()?;
        self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
        let mut params = Vec::new();
        if !self.at(&TokenKind::RParen) {
            loop {
                let pname = self.expect_ident()?;
                self.expect_kind(&TokenKind::Colon, "colon `:`")?;
                let pty = self.parse_type_expr()?;
                params.push((pname, pty));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        // Optional C symbol alias: `extern "C" fn aero_name(...) = "c_symbol";`
        // Defaults to the function name (C symbol names are valid identifiers).
        let extern_symbol = if self.eat(&TokenKind::Eq) {
            match self.advance() {
                Some(Token { kind: TokenKind::Str(s), .. }) => Some(s),
                other => {
                    let (line, col) = match other {
                        Some(t) => (t.line, t.col),
                        None => (start.line, start.col),
                    };
                    return Err(ParseError {
                        msg: "`extern \"C\" fn` symbol alias must be a string `= \"symbol\"`".to_string(),
                        line,
                        col,
                    });
                }
            }
        } else {
            None
        };
        let end = self.expect_kind(&TokenKind::Semi, "semicolon `;` (extern \"C\" declarations have no body)")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::FnDef {
            name,
            type_params: Vec::new(),
            params,
            ret,
            body: Vec::new(),
            is_gpu: false,
            is_extern: true,
            extern_symbol,
            span,
        })
    }

    fn parse_fn(&mut self) -> Result<Stmt, ParseError> {
        self.parse_fn_with_gpu(false)
    }

    fn parse_fn_with_gpu(&mut self, is_gpu: bool) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `fn`");
        let name = self.expect_ident()?;
        // Generic type parameter list: `fn name<T1, T2, ...>(...)`
        let mut type_params = Vec::new();
        if self.eat(&TokenKind::Lt) {
            loop {
                let tp = self.expect_ident()?;
                if type_params.contains(&tp) {
                    return Err(ParseError {
                        msg: format!("duplicate generic type parameter `{tp}`"),
                        line: self
                            .peek()
                            .map(|t| t.line)
                            .unwrap_or(start.line),
                        col: self
                            .peek()
                            .map(|t| t.col)
                            .unwrap_or(start.col),
                    });
                }
                type_params.push(tp);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect_kind(&TokenKind::Gt, "right angle bracket `>`")?;
        }
        self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
        let mut params = Vec::new();
        if !self.at(&TokenKind::RParen) {
            loop {
                let pname = self.expect_ident()?;
                self.expect_kind(&TokenKind::Colon, "colon `:`")?;
                let pty = self.parse_type_expr()?;
                params.push((pname, pty));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        let (body, end) = self.parse_block_with_end()?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::FnDef {
            name,
            type_params,
            params,
            ret,
            body,
            is_gpu,
            is_extern: false,
            extern_symbol: None,
            span,
        })
    }

    fn parse_return(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `return`");
        let value = if self.at(&TokenKind::Semi) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        let end = self.expect_kind(&TokenKind::Semi, "semicolon `;`")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::Return(value, span))
    }

    /// Expression statement, or an lvalue assignment (variable/index/deref).
    fn parse_expr_stmt(&mut self) -> Result<Stmt, ParseError> {
        let lhs = self.parse_expr()?;
        if self.eat(&TokenKind::Eq) {
            let value = self.parse_expr()?;
            let end = self.expect_kind(&TokenKind::Semi, "semicolon `;`")?;
            let start = lhs.span();
            let span = Span {
                line: start.line,
                col: start.col,
                start: start.start,
                end: end.end,
            };
            match lhs {
                Expr::Var(name, _) => Ok(Stmt::Assign { name, value, span }),
                Expr::Index { target, index, .. } => Ok(Stmt::AssignIndex {
                    target,
                    index,
                    value,
                    span,
                }),
                Expr::Deref { target, .. } => Ok(Stmt::AssignDeref {
                    target,
                    value,
                    span,
                }),
                _other => Err(ParseError {
                    msg: "assignment target must be a variable, index, or deref".to_string(),
                    line: span.line,
                    col: span.col,
                }),
            }
        } else {
            let end = self.expect_kind(&TokenKind::Semi, "semicolon `;`")?;
            let start = lhs.span();
            let span = Span {
                line: start.line,
                col: start.col,
                start: start.start,
                end: end.end,
            };
            Ok(Stmt::Expr(lhs, span))
        }
    }

    fn parse_let(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `let`");
        let name = self.expect_ident()?;
        let ty_ann = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        self.expect_kind(&TokenKind::Eq, "operator `=`")?;
        let init = self.parse_expr()?;
        let end = self.expect_kind(&TokenKind::Semi, "semicolon `;`")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::Let {
            name,
            ty_ann,
            init,
            span,
        })
    }

    /// Type annotation: `i32`, `[i64; 3]`, `(i64, bool)`, `&i64`, `&mut i64`, `*i64`.
    fn parse_type_expr(&mut self) -> Result<TypeExpr, ParseError> {
        let tok = match self.peek() {
            Some(t) => t.clone(),
            None => return Err(self.eof_error("expected a type")),
        };
        match &tok.kind {
            TokenKind::Amp => {
                // &T / &mut T
                self.advance();
                let mut_ = self.eat(&TokenKind::Mut);
                let inner = self.parse_type_expr()?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: inner.span().end,
                };
                Ok(TypeExpr::Ref {
                    mut_,
                    inner: Box::new(inner),
                    span,
                })
            }
            TokenKind::Star => {
                // *T raw pointer
                self.advance();
                let inner = self.parse_type_expr()?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: inner.span().end,
                };
                Ok(TypeExpr::Ptr(Box::new(inner), span))
            }
            TokenKind::Ident(name) => {
                self.advance();
                Ok(TypeExpr::Named(name.clone(), span_of(&tok)))
            }
            TokenKind::LBracket => {
                // [T; N]
                self.advance();
                let elem = self.parse_type_expr()?;
                self.expect_kind(&TokenKind::Semi, "semicolon `;`")?;
                let n_tok = self.expect_int()?;
                let n = match n_tok.kind {
                    TokenKind::Int(v) => v as usize,
                    _ => unreachable!("expect_int guaranteed an integer"),
                };
                let end = self.expect_kind(&TokenKind::RBracket, "right bracket `]`")?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: end.end,
                };
                Ok(TypeExpr::Array(Box::new(elem), n, span))
            }
            TokenKind::LParen => {
                // (T, U, ...)
                self.advance();
                let mut elems = Vec::new();
                if !self.at(&TokenKind::RParen) {
                    loop {
                        elems.push(self.parse_type_expr()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                let end = self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: end.end,
                };
                Ok(TypeExpr::Tuple(elems, span))
            }
            other => Err(ParseError {
                msg: format!("expected a type, found {}", other.describe()),
                line: tok.line,
                col: tok.col,
            }),
        }
    }

    fn expect_int(&mut self) -> Result<Token, ParseError> {
        match self.peek() {
            Some(t) if matches!(t.kind, TokenKind::Int(_)) => Ok(self.advance().expect("already checked for integer")),
            Some(t) => Err(ParseError {
                msg: format!("expected integer, found {}", t.kind.describe()),
                line: t.line,
                col: t.col,
            }),
            None => Err(self.eof_error("expected integer")),
        }
    }

    fn parse_print(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `print`");
        self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
        let mut args = Vec::new();
        if self.at(&TokenKind::RParen) {
            return Err(ParseError {
                msg: "print requires at least one argument".to_string(),
                line: start.line,
                col: start.col,
            });
        }
        loop {
            args.push(self.parse_expr()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
        self.expect_kind(&TokenKind::Semi, "semicolon `;`")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::Print(args, span))
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `if`");
        self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
        let cond = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
        let (then_body, then_end) = self.parse_block_with_end()?;
        let mut else_body = Vec::new();
        let mut end = then_end;
        if self.eat(&TokenKind::Else) {
            let (body, body_end) = self.parse_block_with_end()?;
            else_body = body;
            end = body_end;
        }
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::If {
            cond,
            then_body,
            else_body,
            span,
        })
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `while`");
        self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
        let cond = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
        let (body, end) = self.parse_block_with_end()?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::While { cond, body, span })
    }

    /// Parse `{ stmt* }`; returns the statement list and the closing brace
    /// token (used to record the span end).
    fn parse_block_with_end(&mut self) -> Result<(Vec<Stmt>, Token), ParseError> {
        self.expect_kind(&TokenKind::LBrace, "left brace `{`")?;
        let mut stmts = Vec::new();
        while !self.at(&TokenKind::RBrace) {
            if self.at_eof() {
                return Err(self.eof_error("expected `}` to close the block"));
            }
            stmts.push(self.parse_stmt()?);
        }
        let close = self.expect_kind(&TokenKind::RBrace, "right brace `}`")?;
        Ok((stmts, close))
    }

    // ---------- expressions (Pratt) ----------

    /// Infix operator precedence (left-associative). Higher binds tighter,
    /// following C conventions: `* /` > `+ -` > relational `< > <= >=` > equality `== !=` > `AND` > `OR`
    fn infix_bp(kind: &TokenKind) -> Option<(InfixOp, u8)> {
        match kind {
            TokenKind::Plus => Some((InfixOp::Arith(BinOp::Add), 50)),
            TokenKind::Minus => Some((InfixOp::Arith(BinOp::Sub), 50)),
            TokenKind::Star => Some((InfixOp::Arith(BinOp::Mul), 60)),
            TokenKind::Slash => Some((InfixOp::Arith(BinOp::Div), 60)),
            TokenKind::Lt => Some((InfixOp::Cmp(CmpOp::Lt), 40)),
            TokenKind::Gt => Some((InfixOp::Cmp(CmpOp::Gt), 40)),
            TokenKind::Le => Some((InfixOp::Cmp(CmpOp::Le), 40)),
            TokenKind::Ge => Some((InfixOp::Cmp(CmpOp::Ge), 40)),
            TokenKind::EqEq => Some((InfixOp::Cmp(CmpOp::Eq), 30)),
            TokenKind::Ne => Some((InfixOp::Cmp(CmpOp::Ne), 30)),
            TokenKind::AndAnd => Some((InfixOp::Logic(LogicOp::And), 20)),
            TokenKind::OrOr => Some((InfixOp::Logic(LogicOp::Or), 10)),
            _ => None,
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_expr_bp(0)
    }

    fn parse_expr_bp(&mut self, min_bp: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_prefix()?;
        loop {
            let Some((op, lbp)) = self.peek().and_then(|t| Self::infix_bp(&t.kind)) else {
                break;
            };
            if lbp < min_bp {
                break;
            }
            self.advance();
            let rhs = self.parse_expr_bp(lbp + 1)?;
            let lhs_span = lhs.span();
            let span = Span {
                line: lhs_span.line,
                col: lhs_span.col,
                start: lhs_span.start,
                end: rhs.span().end,
            };
            lhs = match op {
                InfixOp::Arith(bin) => Expr::Binary {
                    op: bin,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    span,
                },
                InfixOp::Cmp(cmp) => Expr::Cmp {
                    op: cmp,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    span,
                },
                InfixOp::Logic(logic) => Expr::Logic {
                    op: logic,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    span,
                },
            };
        }
        Ok(lhs)
    }

    /// Prefix: literals, variables/calls, parens/tuples, arrays, unary minus;
    /// then postfix indexing is parsed.
    fn parse_prefix(&mut self) -> Result<Expr, ParseError> {
        let tok = match self.peek() {
            Some(t) => t.clone(),
            None => return Err(self.eof_error("expected an expression")),
        };
        let mut expr = match &tok.kind {
            TokenKind::Int(v) => {
                self.advance();
                Expr::Int(*v, span_of(&tok))
            }
            TokenKind::Str(s) => {
                self.advance();
                Expr::Str(s.clone(), span_of(&tok))
            }
            TokenKind::True => {
                self.advance();
                Expr::Bool(true, span_of(&tok))
            }
            TokenKind::False => {
                self.advance();
                Expr::Bool(false, span_of(&tok))
            }
            TokenKind::Ident(name) => {
                self.advance();
                if self.eat(&TokenKind::LParen) {
                    // Function call name(args...)
                    let mut args = Vec::new();
                    if !self.at(&TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if !self.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    let end = self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                    let span = Span {
                        line: tok.line,
                        col: tok.col,
                        start: tok.start,
                        end: end.end,
                    };
                    Expr::Call {
                        callee: name.clone(),
                        args,
                        span,
                    }
                } else {
                    Expr::Var(name.clone(), span_of(&tok))
                }
            }
            TokenKind::Amp => {
                // Borrow &x / &mut x
                self.advance();
                let mut_ = self.eat(&TokenKind::Mut);
                let target_tok = match self.peek() {
                    Some(t) => t.clone(),
                    None => return Err(self.eof_error("expected a variable to borrow")),
                };
                let name = self.expect_ident()?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: target_tok.end,
                };
                Expr::Borrow {
                    mut_,
                    target: Box::new(Expr::Var(name, span_of(&target_tok))),
                    span,
                }
            }
            TokenKind::Star => {
                // Deref *p (binds tighter than all binary operators)
                self.advance();
                let inner = self.parse_expr_bp(70)?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: inner.span().end,
                };
                Expr::Deref {
                    target: Box::new(inner),
                    span,
                }
            }
            TokenKind::Arena => {
                // Arena literal arena(N)
                self.advance();
                self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
                let n_tok = self.expect_int()?;
                let n = match n_tok.kind {
                    TokenKind::Int(v) if v >= 0 => v as usize,
                    _ => unreachable!("expect_int guaranteed an integer"),
                };
                let end = self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: end.end,
                };
                Expr::ArenaLit(n, span)
            }
            TokenKind::Tensor => {
                // Tensor literal tensor(3, 4, ...) — dims are compile-time integer constants
                self.advance();
                self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
                let mut dims = Vec::new();
                loop {
                    let d_tok = self.expect_int()?;
                    let d = match d_tok.kind {
                        TokenKind::Int(v) if v > 0 => v as usize,
                        TokenKind::Int(v) => {
                            return Err(ParseError {
                                msg: format!("tensor dimension must be positive, got {v}"),
                                line: d_tok.line,
                                col: d_tok.col,
                            });
                        }
                        _ => unreachable!("expect_int guaranteed an integer"),
                    };
                    dims.push(d);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                if dims.is_empty() {
                    return Err(ParseError {
                        msg: "tensor requires at least one dimension".to_string(),
                        line: tok.line,
                        col: tok.col,
                    });
                }
                let end = self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: end.end,
                };
                Expr::TensorLit(dims, span)
            }
            TokenKind::LParen => {
                self.advance();
                if self.at(&TokenKind::RParen) {
                    return Err(ParseError {
                        msg: "empty tuple `()` is not supported".to_string(),
                        line: tok.line,
                        col: tok.col,
                    });
                }
                let first = self.parse_expr()?;
                if self.eat(&TokenKind::Comma) {
                    // Tuple (a, b, ...)
                    let mut elems = vec![first];
                    if !self.at(&TokenKind::RParen) {
                        loop {
                            elems.push(self.parse_expr()?);
                            if !self.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    let end = self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                    let span = Span {
                        line: tok.line,
                        col: tok.col,
                        start: tok.start,
                        end: end.end,
                    };
                    Expr::Tuple(elems, span)
                } else {
                    // Single-element parens: grouping
                    self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                    first
                }
            }
            TokenKind::LBracket => {
                // Array [a, b, ...]
                self.advance();
                let mut elems = Vec::new();
                if !self.at(&TokenKind::RBracket) {
                    loop {
                        elems.push(self.parse_expr()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                let end = self.expect_kind(&TokenKind::RBracket, "right bracket `]`")?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: end.end,
                };
                Expr::Array(elems, span)
            }
            TokenKind::Minus => {
                self.advance();
                // Unary minus binds tighter than all binary operators;
                // the operand is parsed at the minimum precedence 70
                let inner = self.parse_expr_bp(70)?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: inner.span().end,
                };
                Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(inner),
                    span,
                }
            }
            other => {
                return Err(self.error_at_current(&format!(
                    "expected an expression, found {}",
                    other.describe()
                )));
            }
        };

        // postfix: indexing a[i][j]... and method calls a.alloc(n)...
        loop {
            if self.at(&TokenKind::LBracket) {
                self.advance();
                let index = self.parse_expr()?;
                let end = self.expect_kind(&TokenKind::RBracket, "right bracket `]`")?;
                let sp = expr.span();
                let span = Span {
                    line: sp.line,
                    col: sp.col,
                    start: sp.start,
                    end: end.end,
                };
                expr = Expr::Index {
                    target: Box::new(expr),
                    index: Box::new(index),
                    span,
                };
            } else if self.eat(&TokenKind::Dot) {
                let method = self.expect_ident()?;
                self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
                let mut args = Vec::new();
                if !self.at(&TokenKind::RParen) {
                    loop {
                        args.push(self.parse_expr()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                let end = self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                let sp = expr.span();
                let span = Span {
                    line: sp.line,
                    col: sp.col,
                    start: sp.start,
                    end: end.end,
                };
                expr = Expr::MethodCall {
                    recv: Box::new(expr),
                    method,
                    args,
                    span,
                };
            } else {
                break;
            }
        }
        Ok(expr)
    }

    // ---------- helpers ----------

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn at(&self, kind: &TokenKind) -> bool {
        matches!(self.peek(), Some(t) if t.kind == *kind)
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn advance(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn at_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some(t) => match &t.kind {
                TokenKind::Ident(name) => {
                    let name = name.clone();
                    self.advance();
                    Ok(name)
                }
                _ => Err(ParseError {
                    msg: format!("expected identifier, found {}", t.kind.describe()),
                    line: t.line,
                    col: t.col,
                }),
            },
            None => Err(self.eof_error("expected identifier")),
        }
    }

    fn expect_kind(&mut self, kind: &TokenKind, what: &str) -> Result<Token, ParseError> {
        match self.peek() {
            Some(t) if t.kind == *kind => Ok(self.advance().expect("already checked for token")),
            Some(t) => Err(ParseError {
                msg: format!("expected {what}, found {}", t.kind.describe()),
                line: t.line,
                col: t.col,
            }),
            None => Err(self.eof_error(&format!("expected {what}"))),
        }
    }

    fn error_at_current(&self, msg: &str) -> ParseError {
        match self.peek() {
            Some(t) => ParseError {
                msg: msg.to_string(),
                line: t.line,
                col: t.col,
            },
            None => self.eof_error(msg),
        }
    }

    /// Build an error for an unexpected end of source: points at the end of
    /// the last token.
    fn eof_error(&self, msg: &str) -> ParseError {
        match self.tokens.last() {
            Some(t) => ParseError {
                msg: format!("{msg} (source ended)"),
                line: t.line,
                col: t.col + (t.end - t.start) as u32,
            },
            None => ParseError {
                msg: format!("{msg} (source is empty)"),
                line: 0,
                col: 0,
            },
        }
    }
}

fn span_of(tok: &Token) -> Span {
    Span {
        line: tok.line,
        col: tok.col,
        start: tok.start,
        end: tok.end,
    }
}

/// Parse a token stream into an AST.
pub fn parse(tokens: &[Token]) -> Result<Program, ParseError> {
    Parser::new(tokens).parse_program()
}

/// Parse directly from source (lex then parse).
pub fn parse_source(src: &str) -> Result<Program, ParseError> {
    let tokens = aero_lex::lex(src).map_err(|e| ParseError {
        msg: format!("lex error: {}", e.msg),
        line: e.line,
        col: e.col,
    })?;
    parse(&tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_src(src: &str) -> Result<Program, ParseError> {
        let tokens = aero_lex::lex(src).unwrap();
        Parser::new(&tokens).parse_program()
    }

    fn first_expr(program: &Program) -> &Expr {
        match &program.stmts[0] {
            Stmt::Print(args, _) => &args[0],
            Stmt::Expr(expr, _) => expr,
            Stmt::Let { init, .. } => init,
            Stmt::Assign { value, .. } => value,
            Stmt::AssignIndex { value, .. } => value,
            Stmt::AssignDeref { value, .. } => value,
            Stmt::If { cond, .. } => cond,
            Stmt::While { cond, .. } => cond,
            Stmt::Return(Some(v), _) => v,
            Stmt::FnDef { .. } => panic!("FnDef has no expression"),
            Stmt::Return(None, _) => panic!("Return has no value"),
        }
    }

    #[test]
    fn precedence_mul_over_add() {
        let p = parse_src("print(1 + 2 * 3);").unwrap();
        match first_expr(&p) {
            Expr::Binary { op: BinOp::Add, lhs, rhs, .. } => {
                assert!(matches!(**lhs, Expr::Int(1, _)));
                assert!(matches!(**rhs, Expr::Binary { op: BinOp::Mul, .. }));
            }
            other => panic!("expected Add node, got {other:?}"),
        }
    }

    #[test]
    fn left_assoc_sub() {
        // 1 - 2 - 3  =>  (1 - 2) - 3
        let p = parse_src("print(1 - 2 - 3);").unwrap();
        match first_expr(&p) {
            Expr::Binary { op: BinOp::Sub, lhs, rhs, .. } => {
                assert!(matches!(**lhs, Expr::Binary { op: BinOp::Sub, .. }));
                assert!(matches!(**rhs, Expr::Int(3, _)));
            }
            other => panic!("expected Sub node, got {other:?}"),
        }
    }

    #[test]
    fn unary_minus_binds_tighter_than_mul() {
        // -2 * 3  =>  (-2) * 3
        let p = parse_src("print(-2 * 3);").unwrap();
        match first_expr(&p) {
            Expr::Binary { op: BinOp::Mul, lhs, .. } => {
                assert!(matches!(**lhs, Expr::Unary { op: UnOp::Neg, .. }));
            }
            other => panic!("expected Mul node, got {other:?}"),
        }
    }

    #[test]
    fn parens_group() {
        // (1 + 2) * 3  =>  (1+2) is the left operand of the multiply
        let p = parse_src("print((1 + 2) * 3);").unwrap();
        match first_expr(&p) {
            Expr::Binary { op: BinOp::Mul, lhs, .. } => {
                assert!(matches!(**lhs, Expr::Binary { op: BinOp::Add, .. }));
            }
            other => panic!("expected Mul node, got {other:?}"),
        }
    }

    #[test]
    fn let_statement_parsed() {
        let p = parse_src("let x = 1 + 2;").unwrap();
        match &p.stmts[0] {
            Stmt::Let { name, init, .. } => {
                assert_eq!(name, "x");
                assert!(matches!(init, Expr::Binary { op: BinOp::Add, .. }));
            }
            other => panic!("expected let statement, got {other:?}"),
        }
    }

    #[test]
    fn string_literal_parsed() {
        let p = parse_src(r#"print("hi\n");"#).unwrap();
        match first_expr(&p) {
            Expr::Str(s, _) => assert_eq!(s, "hi\n"),
            other => panic!("expected string node, got {other:?}"),
        }
    }

    #[test]
    fn print_multi_args_parsed() {
        let p = parse_src(r#"print("x = %d", 42);"#).unwrap();
        match &p.stmts[0] {
            Stmt::Print(args, _) => {
                assert_eq!(args.len(), 2);
                assert!(matches!(&args[0], Expr::Str(s, _) if s == "x = %d"));
                assert!(matches!(&args[1], Expr::Int(42, _)));
            }
            other => panic!("expected print statement, got {other:?}"),
        }
    }

    #[test]
    fn assign_parsed() {
        let p = parse_src("x = x + 1;").unwrap();
        match &p.stmts[0] {
            Stmt::Assign { name, value, .. } => {
                assert_eq!(name, "x");
                assert!(matches!(value, Expr::Binary { op: BinOp::Add, .. }));
            }
            other => panic!("expected assignment statement, got {other:?}"),
        }
    }

    #[test]
    fn if_else_parsed() {
        let p = parse_src("if (x > 0) { print(1); } else { print(0); }").unwrap();
        match &p.stmts[0] {
            Stmt::If { cond, then_body, else_body, .. } => {
                assert!(matches!(cond, Expr::Cmp { op: CmpOp::Gt, .. }));
                assert_eq!(then_body.len(), 1);
                assert_eq!(else_body.len(), 1);
            }
            other => panic!("expected if statement, got {other:?}"),
        }
    }

    #[test]
    fn while_parsed() {
        let p = parse_src("while (x < 10) { print(x); }").unwrap();
        match &p.stmts[0] {
            Stmt::While { cond, body, .. } => {
                assert!(matches!(cond, Expr::Cmp { op: CmpOp::Lt, .. }));
                assert_eq!(body.len(), 1);
            }
            other => panic!("expected while statement, got {other:?}"),
        }
    }

    #[test]
    fn cmp_precedence() {
        // 1 + 2 < 5  =>  (1 + 2) < 5 (arithmetic binds tighter than comparison)
        let p = parse_src("print(1 + 2 < 5);").unwrap();
        match first_expr(&p) {
            Expr::Cmp { op: CmpOp::Lt, lhs, .. } => {
                assert!(matches!(**lhs, Expr::Binary { op: BinOp::Add, .. }));
            }
            other => panic!("expected comparison node, got {other:?}"),
        }
    }

    #[test]
    fn logic_precedence() {
        // 1 < 2 && 3 < 4  =>  (1 < 2) && (3 < 4)
        let p = parse_src("print(1 < 2 && 3 < 4);").unwrap();
        match first_expr(&p) {
            Expr::Logic { op: LogicOp::And, lhs, rhs, .. } => {
                assert!(matches!(**lhs, Expr::Cmp { op: CmpOp::Lt, .. }));
                assert!(matches!(**rhs, Expr::Cmp { op: CmpOp::Lt, .. }));
            }
            other => panic!("expected logic node, got {other:?}"),
        }
    }

    #[test]
    fn and_tighter_than_or() {
        // 1 || 2 && 3  =>  1 || (2 && 3)
        let p = parse_src("print(1 || 2 && 3);").unwrap();
        match first_expr(&p) {
            Expr::Logic { op: LogicOp::Or, rhs, .. } => {
                assert!(matches!(**rhs, Expr::Logic { op: LogicOp::And, .. }));
            }
            other => panic!("expected or node, got {other:?}"),
        }
    }

    #[test]
    fn fn_def_parsed() {
        let p = parse_src("fn add(a: i64, b: i64) -> i64 { return a + b; }").unwrap();
        match &p.stmts[0] {
            Stmt::FnDef {
                name, params, ret, ..
            } => {
                assert_eq!(name, "add");
                assert_eq!(params.len(), 2);
                assert!(matches!(
                    &params[0].1,
                    TypeExpr::Named(n, _) if n == "i64"
                ));
                assert!(matches!(ret, Some(TypeExpr::Named(n, _)) if n == "i64"));
            }
            other => panic!("expected function definition, got {other:?}"),
        }
    }

    #[test]
    fn let_with_type_annotation() {
        let p = parse_src("let x: i32 = 1;").unwrap();
        match &p.stmts[0] {
            Stmt::Let { ty_ann, init, .. } => {
                assert!(matches!(
                    ty_ann,
                    Some(TypeExpr::Named(n, _)) if n == "i32"
                ));
                assert!(matches!(init, Expr::Int(1, _)));
            }
            other => panic!("expected let statement, got {other:?}"),
        }
    }

    #[test]
    fn call_and_array_parsed() {
        let p = parse_src("let a = [1, 2, 3]; print(a[0]); print(add(1, 2));").unwrap();
        assert!(matches!(
            &p.stmts[0],
            Stmt::Let { init: Expr::Array(elems, _), .. } if elems.len() == 3
        ));
        match &p.stmts[1] {
            Stmt::Print(args, _) => {
                assert!(matches!(
                    &args[0],
                    Expr::Index { index, .. } if matches!(**index, Expr::Int(0, _))
                ));
            }
            other => panic!("expected print, got {other:?}"),
        }
        match &p.stmts[2] {
            Stmt::Print(args, _) => {
                assert!(matches!(
                    &args[0],
                    Expr::Call { callee, args, .. } if callee == "add" && args.len() == 2
                ));
            }
            other => panic!("expected print, got {other:?}"),
        }
    }

    #[test]
    fn tuple_parsed() {
        let p = parse_src("let t = (1, true);").unwrap();
        assert!(matches!(
            &p.stmts[0],
            Stmt::Let {
                init: Expr::Tuple(elems, _),
                ..
            } if elems.len() == 2
        ));
    }

    #[test]
    fn missing_semicolon_errors() {
        let err = parse_src("let x = 1").unwrap_err();
        assert!(err.msg.contains(';'));
        assert_eq!(err.line, 1);
    }

    #[test]
    fn undefined_expr_start_errors() {
        let err = parse_src(";").unwrap_err();
        assert!(err.msg.contains("expression"));
    }

    #[test]
    fn missing_ident_after_let_errors() {
        let err = parse_src("let = 1;").unwrap_err();
        assert!(err.msg.contains("identifier"));
    }
}
