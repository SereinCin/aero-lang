//! 错误信息 2.0：统一的诊断渲染器。
//!
//! 覆盖所有编译阶段（lex / parse / HIR / borrowck / codegen，统一以
//! `aero_ir::AeroError` 暴露）以及静态检查（aero-clippy）的诊断输出。
//! 渲染特性：
//! - 彩色分级：错误=红、警告=黄，其余级别为默认色；遵循 `NO_COLOR`。
//! - 源码片段 + 行号列号：`  --> <file>:<line>:<col>` + 具体源码行 + `^` 定位。
//! - 建议修复：常见错误按消息模式附 `= help:` 提示；clippy 的 suggestion 原样带上。
//!
//! 渲染结果为纯字符串，由调用方写到 stderr；同时提供无 ANSI 变体便于测试与重定向。

/// 一条待渲染的诊断。
#[derive(Debug, Clone)]
pub struct Diag {
    /// 严重级别标签：`error` / `warning` / `style` / `note` 等。
    pub severity: String,
    /// 可选的诊断代码（如编译阶段、clippy 规则名），显示为 `severity[code]`。
    pub code: Option<String>,
    /// 1-based 行号；0 表示无位置（如 IO 错误）。
    pub line: u32,
    /// 1-based 列号。
    pub col: u32,
    /// 主消息。
    pub msg: String,
    /// 建议修复 or 帮助文本（可选）。
    pub hint: Option<String>,
}

impl Diag {
    /// 从 AeroError 构造（主编译流程各阶段共用）。
    pub fn from_aero(e: &aero_ir::AeroError) -> Diag {
        Diag {
            severity: "error".into(),
            code: Some(e.phase.into()),
            line: e.line,
            col: e.col,
            msg: e.msg.clone(),
            hint: suggest(&e.msg),
        }
    }
}

// ---------- ANSI 上色 ----------

const YELLOW: &str = "33";
const CYAN: &str = "36";
const BOLD_RED: &str = "1;31";

/// 是否启用终端颜色：未设置 `NO_COLOR` 且 stderr 是终端。
fn want_color() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

fn paint(s: &str, style: &str, colored: bool) -> String {
    if !colored || s.is_empty() {
        s.to_string()
    } else {
        format!("\u{1b}[{}m{}\u{1b}[0m", style, s)
    }
}

/// 依严重级别取主样式（error=红粗体，warning=黄，其余默认）。
fn severity_style(sev: &str) -> &'static str {
    match sev {
        "error" => BOLD_RED,
        "warning" => YELLOW,
        _ => "",
    }
}

/// 渲染一条诊断；返回含（或不含）ANSI 的完整文本。
pub fn render(d: &Diag, source: &str, filename: &str) -> String {
    let colored = want_color();

    let mut out = String::new();

    // 头部： `error[code]: msg`（severity 分级着色）
    let head = match &d.code {
        Some(c) if !c.is_empty() => format!("{}[{}]: {}", d.severity, c, d.msg),
        _ => format!("{}: {}", d.severity, d.msg),
    };
    let style = severity_style(&d.severity);
    if style.is_empty() {
        out.push_str(&head);
    } else {
        out.push_str(&paint(&head, style, colored));
    }
    out.push('\n');

    // 位置 + 源码片段（仅在已知行号时渲染；避免 I/O 等无位置错误出现空注释行）
    if d.line > 0 {
        // 位置行
        out.push_str(&format!(
            "{} --> {}:{}:{}\n",
            paint("  ", CYAN, colored),
            filename,
            d.line,
            d.col
        ));

        let ln = d.line.to_string();
        let pad = " ".repeat(ln.len());

        // 片段分隔行（gutter）
        out.push_str(&format!("{} |\n", pad));

        // 源码行（gutter 行号用青色）
        let lines: Vec<&str> = source.split('\n').collect();
        if let Some(text) = lines.get((d.line as usize).saturating_sub(1)) {
            let line_num = paint(&ln, CYAN, colored);
            out.push_str(&format!("{} | {}\n", line_num, text));
        }

        // caret 定位：居中于列位置（cap 到行宽，避免越界）
        let caret_col = {
            let text = lines
                .get((d.line as usize).saturating_sub(1))
                .copied()
                .unwrap_or("");
            (d.col as usize).saturating_sub(1).min(text.chars().count())
        };
        out.push_str(&format!("{} | ", paint(&pad, CYAN, colored)));
        out.push_str(&" ".repeat(caret_col));
        out.push_str(&paint("^", BOLD_RED, colored));
        out.push('\n');

        // 建议修复 / 帮助
        if let Some(h) = &d.hint {
            if !h.is_empty() {
                out.push('\n');
                out.push_str(&format!(
                    "{} = help: {}\n",
                    paint("  ", CYAN, colored),
                    h
                ));
            }
        }
    } else if let Some(h) = &d.hint {
        // 无位置但有建议（如 IO 类），直接追加
        if !h.is_empty() {
            out.push_str(&format!("{}= help: {}\n", paint("  ", CYAN, colored), h));
        }
    }

    out
}

/// 便捷入口：从主编译错误（AeroError）渲染。
pub fn render_error(source: &str, filename: &str, e: &aero_ir::AeroError) -> String {
    render(&Diag::from_aero(e), source, filename)
}

// ---------- 建议修复（常见模式） ----------

/// 依据消息文本为常见错误提供一个保守、可确定的修复建议。
///
/// 刻意保持克制：仅对能确定意图的模式给出 hint，其余返回 `None`，
/// 避免给出错误建议造成误导。
fn suggest(msg: &str) -> Option<String> {
    let m = msg.to_ascii_lowercase();
    let has = |kw: &str| m.contains(kw);

    if has("unresolved") || has("undefined") || has("not found") || has("cannot find")
        || has("unknown identifier") || has("not defined")
    {
        return Some(
            "名称未解析：检查拼写、作用域，或是否缺少 `use`/`mod` 导入（`pub` 可见性）."
                .into(),
        );
    }
    if has("borrow") || has("cannot move") {
        return Some("借用检查失败：检查可变/不可变借用是否冲突，或落入借用的生命周期范围.".into());
    }
    if has("expected type") || has("type mismatch") || has("mismatched type") {
        return Some("类型不匹配：核对表达式类型与目标类型（必要时用 `as` 转换或显式标注类型）.".into());
    }
    if has("expected") {
        return Some("语法/结构不完整：按提示补全缺失的 token、标点（`,` `;` `()` `{}`）或关键字."
            .into());
    }
    if has("mutable") && has("immutable") {
        return Some("可变性与引用问题：确认是否需要可变绑定（`let mut`）或可变引用（`&mut`）.".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_position_preamble() {
        let src = "let a = 1\nlet b = foo + 1\n";
        let d = Diag {
            severity: "error".into(),
            code: Some("hir".into()),
            line: 2,
            col: 9,
            msg: "unresolved name `foo`".into(),
            hint: suggest("unresolved name `foo`"),
        };
        let out = render(&d, src, "t.aero");
        assert!(out.contains("t.aero:2:9"), "missing location: {out}");
        assert!(out.contains("let b = foo + 1"), "missing source line: {out}");
        assert!(out.contains("^"), "missing caret: {out}");
        assert!(out.contains("= help:"), "missing hint: {out}");
    }

    #[test]
    fn io_error_without_position_is_plain() {
        let e = aero_ir::AeroError {
            phase: "IO",
            line: 0,
            col: 0,
            msg: "cannot read file x.aero: not found".into(),
        };
        let out = render_error("", "x.aero", &e);
        assert!(out.contains("[IO]"), "missing phase: {out}");
        assert!(!out.contains("--> "), "should not render snippet for no-pos: {out}");
    }

    #[test]
    fn hides_help_when_no_applicable_hint() {
        // 未知模式 → 无 hint，不出现 `= help:`
        let d = Diag {
            severity: "error".into(),
            code: None,
            line: 1,
            col: 1,
            msg: "some arbitrary internal detail".into(),
            hint: None,
        };
        assert!(d.hint.is_none());
        let out = render(&d, "let", "x.aero");
        assert!(!out.contains("= help:"), "unexpected help: {out}");
    }
}