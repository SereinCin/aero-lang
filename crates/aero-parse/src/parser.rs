use aero_lex::token::{Token, TokenKind};

use crate::ast::{
    BinOp, CmpOp, EnumVariant, Expr, LogicOp, MatchArm, MatchPattern, Program, Stmt, TypeExpr,
    UnOp,
};
use crate::span::Span;

/// Unified representation of infix operators, so Pratt parsing can
/// distinguish the AST node kinds.
enum InfixOp {
    Arith(BinOp),
    Cmp(CmpOp),
    Logic(LogicOp),
    /// Type cast `as` (the RHS is a type expression, e.g. `dyn Drawable`).
    Cast,
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
            Some(t) if t.kind == TokenKind::Loop => self.parse_loop(),
            Some(t) if t.kind == TokenKind::For => self.parse_for(),
            Some(t) if t.kind == TokenKind::Fn => self.parse_fn(),
            Some(t) if t.kind == TokenKind::Const => {
                // `const fn ...` is a compile-time function; `const NAME = ...;`
                // is a top-level constant. Peek one token ahead to disambiguate.
                if let Some(nt) = self.peek_nth(1) {
                    if nt.kind == TokenKind::Fn {
                        self.parse_const_fn()
                    } else {
                        self.parse_const_def()
                    }
                } else {
                    self.parse_const_def()
                }
            }
            Some(t) if t.kind == TokenKind::Extern => self.parse_extern_fn(),
            Some(t) if t.kind == TokenKind::Return => self.parse_return(),
            Some(t) if t.kind == TokenKind::Break => {
                let start = self.advance().expect("already checked for `break`");
                let end = self.expect_kind(&TokenKind::Semi, "semicolon `;`")?;
                Ok(Stmt::Break(Span {
                    line: start.line,
                    col: start.col,
                    start: start.start,
                    end: end.end,
                }))
            }
            Some(t) if t.kind == TokenKind::Continue => {
                let start = self.advance().expect("already checked for `continue`");
                let end = self.expect_kind(&TokenKind::Semi, "semicolon `;`")?;
                Ok(Stmt::Continue(Span {
                    line: start.line,
                    col: start.col,
                    start: start.start,
                    end: end.end,
                }))
            }
            Some(t) if t.kind == TokenKind::Match => self.parse_match(),
            Some(t) if t.kind == TokenKind::Struct => self.parse_struct_def(&[]),
            Some(t) if t.kind == TokenKind::Union => self.parse_union_def(),
            Some(t) if t.kind == TokenKind::Enum => self.parse_enum_def(&[]),
            Some(t) if t.kind == TokenKind::Trait => self.parse_trait_def(),
            Some(t) if t.kind == TokenKind::Impl => self.parse_impl_block(),
            Some(t) if t.kind == TokenKind::Hash => self.parse_annotated_def(),
            Some(t) if t.kind == TokenKind::Mod => self.parse_mod(),
            Some(t) if t.kind == TokenKind::Use => self.parse_use(),
            Some(t) if t.kind == TokenKind::Pub => self.parse_pub(),
            _ => self.parse_expr_stmt(),
        }
    }

    // ---------- attribute / type definitions ----------

    /// Attributes before a definition: `#[derive(T1, T2, ...)]`, `#[export]`
    /// and/or `#[py_export]`. Returns the derive names, `#[export]` presence and
    /// `#[py_export]` presence.
    fn parse_attributes(&mut self) -> Result<(Vec<String>, bool, bool), ParseError> {
        let mut derives = Vec::new();
        let mut exported = false;
        let mut py_export = false;
        loop {
            let hash = self
                .advance()
                .ok_or_else(|| self.eof_error("expected `#`"))?;
            self.expect_kind(&TokenKind::LBracket, "left bracket `[`")?;
            let attr = self.expect_ident()?;
            match attr.as_str() {
                "derive" => {
                    self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
                    if !self.at(&TokenKind::RParen) {
                        loop {
                            derives.push(self.expect_ident()?);
                            if !self.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                }
                "export" => {
                    exported = true;
                }
                "py_export" => {
                    exported = true;
                    py_export = true;
                }
                other => {
                    return Err(ParseError {
                        msg: format!(
                            "unsupported attribute `{other}` (supported: `#[derive(...)]`, `#[export]`, `#[py_export]`)"
                        ),
                        line: hash.line,
                        col: hash.col,
                    });
                }
            }
            self.expect_kind(&TokenKind::RBracket, "right bracket `]`")?;
            if !self.at(&TokenKind::Hash) {
                break;
            }
        }
        Ok((derives, exported, py_export))
    }

    /// A statement guarded by attributes: `#[derive(...)] struct/enum ...`,
    /// `#[export] fn ...` or `#[py_export] fn ...`.
    fn parse_annotated_def(&mut self) -> Result<Stmt, ParseError> {
        let (derives, exported, py_export) = self.parse_attributes()?;
        match self.peek() {
            Some(t) if t.kind == TokenKind::Struct => self.parse_struct_def(&derives),
            Some(t) if t.kind == TokenKind::Enum => self.parse_enum_def(&derives),
            Some(t) if t.kind == TokenKind::Fn => {
                self.parse_fn_with_gpu(false, false, exported, py_export)
            }
            _ => Err(self.error_at_current(
                "`#[...]` attributes must be followed by a `struct`, `enum` or `fn` definition",
            )),
        }
    }

    /// `mod name { ... }` (inline module) or `mod name;` (file-backed module).
    fn parse_mod(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `mod`");
        let name = self.expect_ident()?;
        let span = span_of(&start);
        if self.eat(&TokenKind::Semi) {
            return Ok(Stmt::ModFile { name, span });
        }
        self.expect_kind(&TokenKind::LBrace, "`{` to open the module body")?;
        let mut items = Vec::new();
        while !self.at(&TokenKind::RBrace) {
            if self.at_eof() {
                let msg = format!("expected `}}` to close module `{name}`");
                return Err(self.eof_error(&msg));
            }
            items.push(self.parse_stmt()?);
        }
        self.expect_kind(&TokenKind::RBrace, "`}` to close the module body")?;
        Ok(Stmt::ModDef { name, items, span })
    }

    /// `use path::to::Item;` or `use path::to::*;` — import into the current scope.
    fn parse_use(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `use`");
        let mut path = Vec::new();
        loop {
            if self.at(&TokenKind::Star) {
                self.advance();
                path.push("*".to_string());
                break;
            }
            path.push(self.expect_ident()?);
            if !self.eat(&TokenKind::DoubleColon) {
                break;
            }
        }
        if path.is_empty() {
            return Err(self.error_at_current("empty `use` path"));
        }
        let end = self.expect_kind(&TokenKind::Semi, "`;` after the `use` path")?;
        Ok(Stmt::UseDecl {
            path,
            span: Span {
                line: start.line,
                col: start.col,
                start: start.start,
                end: end.end,
            },
        })
    }

    /// `pub <item>` (or `pub(<path>) <item>` for granular visibility, whose path
    /// is accepted and ignored for now). Wraps the item in `Stmt::Pub`.
    fn parse_pub(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `pub`");
        // `pub(crate)` / `pub(super)` / `pub(in path)`: consume the paren group.
        if self.at(&TokenKind::LParen) {
            let mut depth = 0usize;
            loop {
                match self.peek() {
                    Some(t) if t.kind == TokenKind::LParen => {
                        depth += 1;
                        self.advance();
                    }
                    Some(t) if t.kind == TokenKind::RParen => {
                        depth -= 1;
                        self.advance();
                        if depth == 0 {
                            break;
                        }
                    }
                    Some(_) => {
                        self.advance();
                    }
                    None => return Err(self.eof_error("unterminated `pub(...)` visibility")),
                }
            }
        }
        let inner = self.parse_stmt()?;
        Ok(Stmt::Pub(Box::new(inner), span_of(&start)))
    }

    /// `struct Name[<T1, T2, ...>] { field: type, ... }`
    fn parse_struct_def(&mut self, derives: &[String]) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `struct`");
        let name = self.expect_ident()?;
        let type_params = self.parse_optional_type_params()?;
        self.expect_kind(&TokenKind::LBrace, "left brace `{`")?;
        let mut fields = Vec::new();
        while !self.at(&TokenKind::RBrace) {
            if self.at_eof() {
                return Err(self.eof_error("expected `}` to close the struct body"));
            }
            let fname = self.expect_ident()?;
            self.expect_kind(&TokenKind::Colon, "colon `:`")?;
            let fty = self.parse_type_expr()?;
            fields.push((fname, fty));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect_kind(&TokenKind::RBrace, "right brace `}`")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::StructDef {
            name,
            type_params,
            fields,
            derives: derives.to_vec(),
            span,
        })
    }

    /// `union Name { field: type, ... }`
    fn parse_union_def(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `union`");
        let name = self.expect_ident()?;
        self.expect_kind(&TokenKind::LBrace, "left brace `{`")?;
        let mut fields = Vec::new();
        while !self.at(&TokenKind::RBrace) {
            if self.at_eof() {
                return Err(self.eof_error("expected `}` to close the union body"));
            }
            let fname = self.expect_ident()?;
            self.expect_kind(&TokenKind::Colon, "colon `:`")?;
            let fty = self.parse_type_expr()?;
            fields.push((fname, fty));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect_kind(&TokenKind::RBrace, "right brace `}`")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::UnionDef { name, fields, span })
    }

    /// `enum Name[<T1, T2, ...>] { Variant, Variant(type), ... }`
    fn parse_enum_def(&mut self, derives: &[String]) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `enum`");
        let name = self.expect_ident()?;
        let type_params = self.parse_optional_type_params()?;
        self.expect_kind(&TokenKind::LBrace, "left brace `{`")?;
        let mut variants = Vec::new();
        while !self.at(&TokenKind::RBrace) {
            if self.at_eof() {
                return Err(self.eof_error("expected `}` to close the enum body"));
            }
            let vstart = self
                .peek()
                .map(|t| (t.line, t.col, t.start))
                .ok_or_else(|| self.eof_error("expected a variant name"))?;
            let vname = self.expect_ident()?;
            let mut payload = None;
            let mut vend = 0;
            if self.eat(&TokenKind::LParen) {
                let pty = self.parse_type_expr()?;
                let ptok = self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                vend = ptok.end;
                payload = Some(pty);
            } else {
                vend = self
                    .peek()
                    .map(|t| t.start)
                    .unwrap_or_else(|| vstart.2);
            }
            variants.push(EnumVariant {
                name: vname,
                payload,
                span: Span {
                    line: vstart.0,
                    col: vstart.1,
                    start: vstart.2,
                    end: vend,
                },
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect_kind(&TokenKind::RBrace, "right brace `}`")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::EnumDef {
            name,
            type_params,
            variants,
            derives: derives.to_vec(),
            span,
        })
    }

    /// `trait Name[<T1, T2, ...>] { fn method(params) -> ret; ... }`
    fn parse_trait_def(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `trait`");
        let name = self.expect_ident()?;
        // Optional generic parameter list: `trait Name<T1, T2, ...>`
        let type_params = self.parse_optional_type_params()?;
        self.expect_kind(&TokenKind::LBrace, "left brace `{`")?;
        let mut methods = Vec::new();
        let mut assoc_types = Vec::new();
        while !self.at(&TokenKind::RBrace) {
            if self.at_eof() {
                return Err(self.eof_error("expected `}` to close the trait body"));
            }
            if self.at(&TokenKind::Type) {
                // Associated type declaration: `type Item;`
                self.advance();
                let aname = self.expect_ident()?;
                self.expect_kind(&TokenKind::Semi, "semicolon `;` after associated type")?;
                assoc_types.push(aname.clone());
                continue;
            }
            let mstart = self
                .expect_kind(&TokenKind::Fn, "`fn` in trait body")?;
            let mname = self.expect_ident()?;
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
            let mend = self.expect_kind(&TokenKind::Semi, "semicolon `;` (trait methods have no body)")?;
            methods.push(crate::ast::TraitMethodSig {
                name: mname,
                params,
                ret,
                span: Span {
                    line: mstart.line,
                    col: mstart.col,
                    start: mstart.start,
                    end: mend.end,
                },
            });
        }
        let end = self.expect_kind(&TokenKind::RBrace, "right brace `}`")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::TraitDef {
            name,
            type_params,
            assoc_types,
            methods,
            span,
        })
    }

    /// `impl [<T1, T2, ...>] [Trait[<Args>] for] Type[<Args>] { fn ... { ... } ... }`
    fn parse_impl_block(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `impl`");
        // Optional generic header: `impl<T, U, ...>`
        let type_params = self.parse_optional_type_params()?;
        // First identifier: either the trait name (`impl<T> Trait for Type`) or the
        // target type name (`impl<T> Type`). A `<...>` following it is either the
        // trait's generic arguments (`impl Trait<Args> for Type`) or the target
        // type's generic arguments (`impl<T> Type<Args>`).
        let first_ident = self.expect_ident()?;
        let mut trait_args: Vec<TypeExpr> = Vec::new();
        let (trait_name, type_name) = if self.at(&TokenKind::Lt) {
            // Generic argument list after the first ident; decide by what follows.
            let args = self.parse_generic_arg_list()?;
            if self.eat(&TokenKind::For) {
                // `impl Trait<Args> for Type`
                trait_args = args;
                let type_name = self.parse_impl_type_name(&type_params)?;
                (Some(first_ident), type_name)
            } else {
                // `impl [<T>] Type<Args>` — inherent generic impl
                let type_name =
                    self.validate_impl_type_args(first_ident.clone(), args, &type_params)?;
                (None, type_name)
            }
        } else if self.eat(&TokenKind::For) {
            // `impl [<T>] Trait for Type`
            let type_name = self.parse_impl_type_name(&type_params)?;
            (Some(first_ident), type_name)
        } else {
            // `impl Type` — inherent impl on a plain type
            (None, first_ident)
        };
        self.expect_kind(&TokenKind::LBrace, "left brace `{`")?;
        let mut methods = Vec::new();
        let mut assoc_types = Vec::new();
        while !self.at(&TokenKind::RBrace) {
            if self.at_eof() {
                return Err(self.eof_error("expected `}` to close the impl block"));
            }
            if self.at(&TokenKind::Type) {
                // Associated type binding: `type Item = i64;`
                self.advance(); // consume `type`
                let aname = self.expect_ident()?;
                self.expect_kind(&TokenKind::Eq, "`=` after associated type name")?;
                let aty = self.parse_type_expr()?;
                self.expect_kind(&TokenKind::Semi, "semicolon `;` after associated type binding")?;
                assoc_types.push((aname, aty));
                continue;
            }
            // Each method is a full FnDef (with body)
            if !self.at(&TokenKind::Fn) {
                let tok = self.peek().map(|t| (t.line, t.col, t.kind.clone()));
                let (line, col, kind) = tok.unwrap_or((start.line, start.col, TokenKind::Fn));
                return Err(ParseError {
                    msg: format!("expected `fn` in impl body, found {}", kind.describe()),
                    line,
                    col,
                });
            }
            methods.push(self.parse_fn()?);
        }
        let end = self.expect_kind(&TokenKind::RBrace, "right brace `}`")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::ImplBlock {
            type_params,
            trait_name,
            trait_args,
            type_name,
            assoc_types,
            methods,
            span,
        })
    }

    /// Parse a generic argument list `<Arg1, Arg2, ...>` where each argument is a
    /// full type expression. Consumes the surrounding angle brackets.
    fn parse_generic_arg_list(&mut self) -> Result<Vec<TypeExpr>, ParseError> {
        self.advance(); // consume `<`
        let mut args = Vec::new();
        if !self.at(&TokenKind::Gt) {
            loop {
                args.push(self.parse_type_expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_kind(&TokenKind::Gt, "right angle bracket `>`")?;
        Ok(args)
    }

    /// Validate the generic argument list of an inherent impl target
    /// (`impl<T> Type<Args>`): args must be exactly the impl's generic parameters
    /// in order, so the impl covers all instances. Returns the bare type name.
    fn validate_impl_type_args(
        &mut self,
        type_name: String,
        args: Vec<TypeExpr>,
        type_params: &[String],
    ) -> Result<String, ParseError> {
        let names: Vec<String> = args
            .iter()
            .map(|a| match a {
                TypeExpr::Named(n, _) => n.clone(),
                _ => String::new(),
            })
            .collect();
        if type_params.is_empty() {
            return Err(ParseError {
                msg: format!(
                    "impl for `{type_name}<{}>` is not supported: generic impls need an `impl<T>` header (or write `impl {type_name}` for the plain type)",
                    names.join(", ")
                ),
                line: self.peek().map(|t| t.line).unwrap_or(0),
                col: self.peek().map(|t| t.col).unwrap_or(0),
            });
        }
        if names.len() != type_params.len() || !names.iter().zip(type_params).all(|(a, b)| a == b) {
            return Err(ParseError {
                msg: format!(
                    "impl target `{type_name}<{}>` must use the impl's generic parameters `<{}>` in the same order",
                    names.join(", "),
                    type_params.join(", ")
                ),
                line: self.peek().map(|t| t.line).unwrap_or(0),
                col: self.peek().map(|t| t.col).unwrap_or(0),
            });
        }
        Ok(type_name)
    }

    /// Parse the impl target type: a bare name, or `Name<Args>` whose arguments
    /// must be exactly the impl's generic parameters (checked).
    fn parse_impl_type_name(&mut self, type_params: &[String]) -> Result<String, ParseError> {
        let type_name = self.expect_ident()?;
        if self.at(&TokenKind::Lt) {
            let args = self.parse_generic_arg_list()?;
            self.validate_impl_type_args(type_name.clone(), args, type_params)?;
        }
        Ok(type_name)
    }

    /// `for (x in iter) { ... }`
    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `for`");
        self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
        let var = self.expect_ident()?;
        self.expect_kind(&TokenKind::In, "`in`")?;
        let iter = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
        let (body, end) = self.parse_block_with_end()?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::For { var, iter, body, span })
    }

    /// `match (scrutinee) { arm, arm, ... }` — arms are `pattern => { body }`
    fn parse_match(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `match`");
        self.expect_kind(&TokenKind::LParen, "left paren `(`")?;
        let scrutinee = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
        self.expect_kind(&TokenKind::LBrace, "left brace `{`")?;
        let mut arms = Vec::new();
        while !self.at(&TokenKind::RBrace) {
            if self.at_eof() {
                return Err(self.eof_error("expected `}` to close the match body"));
            }
            let pat_start = self
                .peek()
                .map(|t| (t.line, t.col, t.start))
                .ok_or_else(|| self.eof_error("expected a match pattern"))?;
            let pat = self.parse_match_pattern()?;
            self.expect_kind(&TokenKind::FatArrow, "`=>`")?;
            let (body, arm_end) = self.parse_block_with_end()?;
            arms.push(MatchArm {
                pattern: pat,
                body,
                span: Span {
                    line: pat_start.0,
                    col: pat_start.1,
                    start: pat_start.2,
                    end: arm_end.end,
                },
            });
            // Optional trailing comma between arms (arms may also be newline-separated)
            self.eat(&TokenKind::Comma);
        }
        let end = self.expect_kind(&TokenKind::RBrace, "right brace `}`")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::Match { scrutinee, arms, span })
    }

    /// Match patterns: `_`, literals, a binding name, or an enum variant
    /// (`Variant`, `Variant(x)`, `Enum::Variant`, `Enum::Variant(x)`).
    fn parse_match_pattern(&mut self) -> Result<MatchPattern, ParseError> {
        let tok = match self.peek() {
            Some(t) => t.clone(),
            None => return Err(self.eof_error("expected a match pattern")),
        };
        match &tok.kind {
            TokenKind::Underscore => {
                self.advance();
                Ok(MatchPattern::Wildcard)
            }
            TokenKind::Int(v) => {
                self.advance();
                Ok(MatchPattern::IntLit(*v))
            }
            TokenKind::True | TokenKind::False => {
                self.advance();
                Ok(MatchPattern::BoolLit(tok.kind == TokenKind::True))
            }
            TokenKind::Char(c) => {
                self.advance();
                Ok(MatchPattern::CharLit(*c))
            }
            TokenKind::Str(s) => {
                self.advance();
                Ok(MatchPattern::StrLit(s.clone()))
            }
            TokenKind::Ident(name) => {
                self.advance();
                if self.eat(&TokenKind::DoubleColon) {
                    // Enum::Variant / Enum::Variant(x)
                    let variant = self.expect_ident()?;
                    let bind = if self.eat(&TokenKind::LParen) {
                        let b = match self.peek() {
                            Some(t) if t.kind == TokenKind::RParen => {
                                return Err(ParseError {
                                    msg: "enum variant pattern needs a binding or `_` in parens"
                                        .to_string(),
                                    line: t.line,
                                    col: t.col,
                                });
                            }
                            Some(t) if t.kind == TokenKind::Underscore => None,
                            _ => Some(self.expect_ident()?),
                        };
                        self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                        b
                    } else {
                        None
                    };
                    Ok(MatchPattern::EnumVariant {
                        enum_name: Some(name.clone()),
                        variant,
                        bind,
                        span: span_of(&tok),
                    })
                } else if self.eat(&TokenKind::LParen) {
                    // Variant(x) — enum name resolved against scrutinee type later
                    let bind = match self.peek() {
                        Some(t) if t.kind == TokenKind::Underscore => None,
                        _ => Some(self.expect_ident()?),
                    };
                    self.expect_kind(&TokenKind::RParen, "right paren `)`")?;
                    Ok(MatchPattern::EnumVariant {
                        enum_name: None,
                        variant: name.clone(),
                        bind,
                        span: span_of(&tok),
                    })
                } else {
                    // Bare variant name (resolved against the scrutinee's enum type by
                    // lowering); a bare name is never a plain binding pattern in Aero.
                    Ok(MatchPattern::EnumVariant {
                        enum_name: None,
                        variant: name.clone(),
                        bind: None,
                        span: span_of(&tok),
                    })
                }
            }
            other => Err(ParseError {
                msg: format!("expected a match pattern, found {}", other.describe()),
                line: tok.line,
                col: tok.col,
            }),
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
            "gpu" => self.parse_fn_with_gpu(true, false, false, false),
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
            lifetimes: Vec::new(),
            type_params: Vec::new(),
            trait_bounds: Vec::new(),
            params,
            ret,
            body: Vec::new(),
            is_gpu: false,
            is_const: false,
            is_extern: true,
            extern_symbol,
            exported: false,
            py_export: false,
            span,
        })
    }

    fn parse_fn(&mut self) -> Result<Stmt, ParseError> {
        self.parse_fn_with_gpu(false, false, false, false)
    }

    /// `const fn <name>(<params>) [-> <ret>] { ... }` — a function whose body may
    /// be evaluated at compile time when called with constant arguments.
    fn parse_const_fn(&mut self) -> Result<Stmt, ParseError> {
        self.advance().expect("already checked for `const`");
        if !self.at(&TokenKind::Fn) {
            let (line, col) = self
                .peek()
                .map(|t| (t.line, t.col))
                .unwrap_or_else(|| (0, 0));
            return Err(ParseError {
                msg: "after `const` must come `fn`".to_string(),
                line,
                col,
            });
        }
        self.parse_fn_with_gpu(false, true, false, false)
    }

    /// `const NAME[: TYPE] = <expr>;` — a top-level constant. The value is
    /// evaluated at compile time and references are filled in (Phase P0-3).
    fn parse_const_def(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `const`");
        let name = self.expect_ident()?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        self.expect_kind(&TokenKind::Eq, "`=` in a const definition")?;
        let value = self.parse_expr()?;
        let end = self.expect_kind(&TokenKind::Semi, "semicolon `;` after the constant value")?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::ConstDef {
            name,
            ty,
            value: Box::new(value),
            span,
        })
    }

    fn parse_fn_with_gpu(
        &mut self,
        is_gpu: bool,
        is_const: bool,
        exported: bool,
        py_export: bool,
    ) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `fn`");
        let name = self.expect_ident()?;
        // Named lifetime parameters: `fn foo<'a, 'b>(...)`. These appear before the
        // generic type parameters in the same angle-bracket group.
        let mut lifetimes = Vec::new();
        // Generic type parameter list: `fn name<T1: Bound, T2, ...>(...)`
        let mut type_params = Vec::new();
        let mut trait_bounds = Vec::new();
        if self.eat(&TokenKind::Lt) {
            loop {
                if self.at_lifetime() {
                    let lt = match self.advance() {
                        Some(t) => match t.kind {
                            TokenKind::Lifetime(s) => s,
                            _ => unreachable!("peeked Lifetime"),
                        },
                        None => break,
                    };
                    if lifetimes.contains(&lt) {
                        return Err(ParseError {
                            msg: format!("duplicate lifetime parameter `'{lt}`"),
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
                    lifetimes.push(lt);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                    continue;
                }
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
                type_params.push(tp.clone());
                // Optional trait bound: `T: TraitName`
                if self.eat(&TokenKind::Colon) {
                    let bound = self.expect_ident()?;
                    trait_bounds.push((tp, bound));
                }
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
            lifetimes,
            type_params,
            trait_bounds,
            params,
            ret,
            body,
            is_gpu,
            is_const,
            is_extern: false,
            extern_symbol: None,
            exported,
            py_export,
            span,
        })
    }

    /// Optional generic parameter list `<T1, T2, ...>` (empty if absent).
    fn parse_optional_type_params(&mut self) -> Result<Vec<String>, ParseError> {
        let mut type_params = Vec::new();
        if self.eat(&TokenKind::Lt) {
            loop {
                let tp = self.expect_ident()?;
                if type_params.contains(&tp) {
                    return Err(self.error_at_current(&format!(
                        "duplicate generic type parameter `{tp}`"
                    )));
                }
                type_params.push(tp);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect_kind(&TokenKind::Gt, "right angle bracket `>`")?;
        }
        Ok(type_params)
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
                Expr::Field { target, field, .. } => Ok(Stmt::AssignField {
                    target,
                    field,
                    value,
                    span,
                }),
                _other => Err(ParseError {
                    msg: "assignment target must be a variable, index, deref, or field".to_string(),
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
        let mut_ = self.eat(&TokenKind::Mut);
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
            mut_,
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
            TokenKind::Dyn => {
                // dyn TraitName — dynamic trait object type
                self.advance();
                let name_tok = match self.peek() {
                    Some(t) => t.clone(),
                    None => return Err(self.eof_error("expected trait name after `dyn`")),
                };
                let name = self.expect_ident()?;
                let span = Span {
                    line: tok.line,
                    col: tok.col,
                    start: tok.start,
                    end: name_tok.end,
                };
                Ok(TypeExpr::Dyn { name, span })
            }
            TokenKind::Amp => {
                // &T / &mut T / &'a T / &'a mut T
                self.advance();
                // Optional named lifetime: `&'a T`
                let lifetime = if self.at_lifetime() {
                    match self.advance() {
                        Some(t) => match t.kind {
                            TokenKind::Lifetime(s) => Some(s),
                            _ => None,
                        },
                        None => None,
                    }
                } else {
                    None
                };
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
                    lifetime,
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
                if name == "Self" && self.at(&TokenKind::DoubleColon) {
                    // Qualified associated type: `Self::Item`
                    self.advance(); // consume `::`
                    let aname = self.expect_ident()?;
                    let aend = self
                        .peek()
                        .map(|t| t.start.saturating_sub(1))
                        .unwrap_or(tok.start + 1);
                    let span = Span {
                        line: tok.line,
                        col: tok.col,
                        start: tok.start,
                        end: aend,
                    };
                    return Ok(TypeExpr::Path {
                        root: "Self".to_string(),
                        name: aname,
                        span,
                    });
                }
                if name == "Self" {
                    // `Self` in a trait method signature: replaced with the impl target type
                    Ok(TypeExpr::Named("Self".to_string(), span_of(&tok)))
                } else if self.at(&TokenKind::Lt) {
                    // Generic type application: Name<Arg1, Arg2, ...>
                    let args = self.parse_generic_arg_list()?;
                    let last_end = args
                        .last()
                        .map(|a| a.span().end)
                        .unwrap_or(tok.start + 1);
                    let span = Span {
                        line: tok.line,
                        col: tok.col,
                        start: tok.start,
                        end: last_end,
                    };
                    Ok(TypeExpr::Generic {
                        name: name.clone(),
                        args,
                        span,
                    })
                } else {
                    Ok(TypeExpr::Named(name.clone(), span_of(&tok)))
                }
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
        let mut end = then_end.end;
        if self.eat(&TokenKind::Else) {
            // Support `else if` chain — parse as nested `if` inside `else` body
            if matches!(self.peek(), Some(t) if t.kind == TokenKind::If) {
                let stmt = self.parse_if()?;
                end = stmt.span().end;
                else_body = vec![stmt];
            } else {
                let (body, body_end) = self.parse_block_with_end()?;
                else_body = body;
                end = body_end.end;
            }
        }
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end,
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

    /// Parse `loop { ... }` (an infinite loop; exit via `break;`).
    fn parse_loop(&mut self) -> Result<Stmt, ParseError> {
        let start = self.advance().expect("already checked for `loop`");
        let (body, end) = self.parse_block_with_end()?;
        let span = Span {
            line: start.line,
            col: start.col,
            start: start.start,
            end: end.end,
        };
        Ok(Stmt::Loop { body, span })
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
    /// following C conventions: `* /` > `+ -` > shift `<< >>` > relational
    /// `< > <= >=` > equality `== !=` > bitwise `& ^ |` > `&&` > `||`
    fn infix_bp(kind: &TokenKind) -> Option<(InfixOp, u8)> {
        match kind {
            TokenKind::Plus => Some((InfixOp::Arith(BinOp::Add), 50)),
            TokenKind::Minus => Some((InfixOp::Arith(BinOp::Sub), 50)),
            TokenKind::Star => Some((InfixOp::Arith(BinOp::Mul), 60)),
            TokenKind::Slash => Some((InfixOp::Arith(BinOp::Div), 60)),
            TokenKind::Percent => Some((InfixOp::Arith(BinOp::Rem), 60)),
            TokenKind::Shl => Some((InfixOp::Arith(BinOp::Shl), 45)),
            TokenKind::Shr => Some((InfixOp::Arith(BinOp::Shr), 45)),
            TokenKind::Lt => Some((InfixOp::Cmp(CmpOp::Lt), 40)),
            TokenKind::Gt => Some((InfixOp::Cmp(CmpOp::Gt), 40)),
            TokenKind::Le => Some((InfixOp::Cmp(CmpOp::Le), 40)),
            TokenKind::Ge => Some((InfixOp::Cmp(CmpOp::Ge), 40)),
            TokenKind::EqEq => Some((InfixOp::Cmp(CmpOp::Eq), 30)),
            TokenKind::Ne => Some((InfixOp::Cmp(CmpOp::Ne), 30)),
            TokenKind::Amp => Some((InfixOp::Arith(BinOp::BitAnd), 25)),
            TokenKind::Caret => Some((InfixOp::Arith(BinOp::BitXor), 24)),
            TokenKind::Pipe => Some((InfixOp::Arith(BinOp::BitOr), 23)),
            TokenKind::AndAnd => Some((InfixOp::Logic(LogicOp::And), 20)),
            TokenKind::OrOr => Some((InfixOp::Logic(LogicOp::Or), 10)),
            // `as` binds looser than arithmetic/comparison but tighter than
            // logical operators, so `x + 1 as dyn T` groups as `x + (1 as dyn T)`.
            TokenKind::As => Some((InfixOp::Cast, 15)),
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
            if matches!(op, InfixOp::Cast) {
                // `expr as Type` — the RHS is a type expression, not an expression
                let ty = self.parse_type_expr()?;
                let lhs_span = lhs.span();
                let span = Span {
                    line: lhs_span.line,
                    col: lhs_span.col,
                    start: lhs_span.start,
                    end: ty.span().end,
                };
                lhs = Expr::Cast {
                    target: Box::new(lhs),
                    ty,
                    span,
                };
                continue;
            }
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
                // Handled above: `as` casts `continue` before reaching this match.
                InfixOp::Cast => unreachable!("cast handled above"),
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
            TokenKind::Float(v) => {
                self.advance();
                Expr::Float(*v, span_of(&tok))
            }
            TokenKind::Char(c) => {
                self.advance();
                Expr::Char(*c, span_of(&tok))
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
                } else if self.eat(&TokenKind::LBrace) {
                    // Struct literal Name { field: expr, ... }
                    let mut fields = Vec::new();
                    while !self.at(&TokenKind::RBrace) {
                        let fname = self.expect_ident()?;
                        self.expect_kind(&TokenKind::Colon, "colon `:`")?;
                        let fexpr = self.parse_expr()?;
                        fields.push((fname, fexpr));
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    let end = self.expect_kind(&TokenKind::RBrace, "right brace `}`")?;
                    let span = Span {
                        line: tok.line,
                        col: tok.col,
                        start: tok.start,
                        end: end.end,
                    };
                    Expr::StructLit {
                        name: name.clone(),
                        fields,
                        span,
                    }
                } else if self.eat(&TokenKind::DoubleColon) {
                    // Module path / enum variant: `a::b::c(...)` / `Enum::Variant` /
                    // `Enum::Variant(expr)` / native `String::new()`.
                    let mut path = vec![name.clone()];
                    loop {
                        let seg = self.expect_ident()?;
                        path.push(seg);
                        if !self.eat(&TokenKind::DoubleColon) {
                            break;
                        }
                    }
                    if self.eat(&TokenKind::LParen) {
                        // Path call: `a::b::c(args...)` or native/enum constructor `T::X(...)`.
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
                        Expr::PathCall { path, args, span }
                    } else if path.len() == 2 {
                        // Enum variant reference `Enum::Variant` (no payload).
                        let sp = span_of(&tok);
                        let span = Span {
                            line: sp.line,
                            col: sp.col,
                            start: sp.start,
                            end: sp.start + 1,
                        };
                        Expr::EnumLit {
                            name: path[0].clone(),
                            variant: path[1].clone(),
                            arg: None,
                            span,
                        }
                    } else {
                        // A bare multi-segment path without a call (e.g. `a::b::c`) is not
                        // supported yet.
                        let sp = span_of(&tok);
                        return Err(ParseError {
                            msg: format!(
                                "expected `(` after path `{}` (bare multi-segment paths are not yet supported)",
                                path.join("::")
                            ),
                            line: sp.line,
                            col: sp.col,
                        });
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
                // Tensor literal tensor(3, 4, ...) or tensor<f64>(3, 4, ...) — dims
                // are compile-time integer constants. The optional `<elem>` type
                // defaults to `i64` when omitted.
                self.advance();
                let elem = if self.at(&TokenKind::Lt) {
                    let args = self.parse_generic_arg_list()?;
                    if args.len() != 1 {
                        return Err(ParseError {
                            msg: "`tensor<...>` takes exactly one element type (e.g. `tensor<f64>(2, 3)`)"
                                .to_string(),
                            line: tok.line,
                            col: tok.col,
                        });
                    }
                    Some(args.into_iter().next().unwrap())
                } else {
                    None
                };
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
                Expr::TensorLit(dims, elem, span)
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
                // Array literal `[a, b, ...]`, or repeat-initialization `[value; N]`.
                self.advance();
                let mut elems = Vec::new();
                if !self.at(&TokenKind::RBracket) {
                    elems.push(self.parse_expr()?);
                    if self.eat(&TokenKind::Semi) {
                        // `[expr; N]`: N copies of a literal value. The count must be
                        // a constant integer literal (array sizes are static). Only
                        // literal elements are expanded so no side effect is duplicated.
                        let count = self.parse_expr()?;
                        let n = match &count {
                            Expr::Int(n, _) => *n,
                            other => {
                                let sp = other.span();
                                return Err(ParseError {
                                    msg: "array repeat count must be an integer literal"
                                        .to_string(),
                                    line: sp.line,
                                    col: sp.col,
                                });
                            }
                        };
                        if n < 0 {
                            return Err(self.error_at_current(
                                "array repeat count cannot be negative",
                            ));
                        }
                        let value = elems[0].clone();
                        if !matches!(
                            value,
                            Expr::Int(..)
                                | Expr::Float(..)
                                | Expr::Char(..)
                                | Expr::Bool(..)
                                | Expr::Str(..)
                        ) {
                            return Err(self.error_at_current(
                                "repeat-initialized array element must be a literal \
                                 (`[0; N]`, `[0.0; N]`, `[\"\"; N]`, ...)",
                            ));
                        }
                        elems.clear();
                        for _ in 0..n {
                            elems.push(value.clone());
                        }
                    } else {
                        while self.eat(&TokenKind::Comma) {
                            elems.push(self.parse_expr()?);
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
                let member = self.expect_ident()?;
                if self.eat(&TokenKind::LParen) {
                    // Method call `recv.method(args...)`
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
                        method: member,
                        args,
                        span,
                    };
                } else {
                    // Field access `recv.field` (no parentheses)
                    let sp = expr.span();
                    let span = Span {
                        line: sp.line,
                        col: sp.col,
                        start: sp.start,
                        end: sp.end,
                    };
                    expr = Expr::Field {
                        target: Box::new(expr),
                        field: member,
                        span,
                    };
                }
            } else if self.eat(&TokenKind::Question) {
                // Try `expr?`: unwrap Result<T, E>, propagating the error.
                let sp = expr.span();
                let span = Span {
                    line: sp.line,
                    col: sp.col,
                    start: sp.start,
                    end: sp.end + 1,
                };
                expr = Expr::Try {
                    target: Box::new(expr),
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

    fn peek_nth(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.pos + n)
    }

    fn at(&self, kind: &TokenKind) -> bool {
        matches!(self.peek(), Some(t) if t.kind == *kind)
    }

    /// Whether the current token is a lifetime parameter (`'a`). `Lifetime`
    /// carries a `String` payload, so it cannot be matched with `at`.
    fn at_lifetime(&self) -> bool {
        matches!(self.peek(), Some(t) if matches!(t.kind, TokenKind::Lifetime(_)))
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
            Stmt::AssignField { value, .. } => value,
            Stmt::If { cond, .. } => cond,
            Stmt::While { cond, .. } => cond,
            Stmt::Return(Some(v), _) => v,
            Stmt::FnDef { .. } => panic!("FnDef has no expression"),
            Stmt::Return(None, _) => panic!("Return has no value"),
            Stmt::For { iter, .. } => iter,
            other => panic!("statement has no single expression: {other:?}"),
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
    fn std_impl_then_top_level_fn_with_ref_mut_generic() {
        // Matches std.aero: LinkedList impl<T> ends, then a top-level function takes
        // `&mut Vec<i64>, pred: Fn` as params.
        let src = r#"
struct LinkedList<T> { vals: Vec<i64> }
impl<T> LinkedList<T> {
    fn clear(l: &mut LinkedList<T>) { l.head = -1; }
}
fn _filter_impl(v: &mut Vec<i64>, pred: Fn) -> Vec<i64> { return Vec::new(); }
"#;
        let p = parse_src(src).unwrap();
        match &p.stmts[2] {
            Stmt::FnDef { name, params, .. } => {
                assert_eq!(name, "_filter_impl");
                assert_eq!(params.len(), 2);
            }
            other => panic!("expected top-level fn, got {other:?}"),
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
