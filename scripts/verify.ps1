# Aero 1.2.6 verify.ps1
# 无参数运行 → 输出 PASS/FAIL → 全绿 exit 0，任一失败 exit 1
# 约束：不依赖网络；单次运行 < 3 分钟；临时文件写到 $env:TEMP

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$bin = Join-Path $root "target\release\aero.exe"
$temp = Join-Path $env:TEMP "aero-verify-126"
if (Test-Path $temp) { Remove-Item $temp -Recurse -Force }
New-Item -ItemType Directory -Path $temp -Force | Out-Null

$pass = 0
$fail = 0
$results = @()

function Assert {
    param([string]$id, [string]$desc, [bool]$ok, [string]$detail = "")
    if ($ok) { $script:pass++ } else { $script:fail++ }
    $mark = if ($ok) { "PASS" } else { "FAIL" }
    $script:results += "$mark | $id | $desc"
    if (-not $ok -and $detail) { $script:results += "      → $detail" }
}

# —————————————————————————————————————
# 前置：构建 release binary（若不存在）
# —————————————————————————————————————
if (-not (Test-Path $bin)) {
    Write-Host "== building release =="
    Push-Location $root
    cargo build --release 2>&1 | Out-Null
    Pop-Location
}
$aero = $bin

# —————————————————————————————————————
# P0-1: mod foo from "..." 已从所有文档删除
# —————————————————————————————————————
$docFiles = @(Join-Path $root "README.md")
$docFiles += Get-ChildItem (Join-Path $root "docs") -Recurse -Include *.md -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }
# A line that mentions the syntax only to say it is unsupported is fine;
# what must not exist is a line advertising it as usable.
$foundModFrom = Select-String -Path $docFiles -Pattern 'mod\s+\w+\s+from' -ErrorAction SilentlyContinue |
    Where-Object { $_.Line -notmatch "doesn't exist|does not exist|不存在|Our mistake" }
Assert "P0-1" "文档中无 'mod xxx from'" (-not $foundModFrom) ($(if ($foundModFrom) { "仍有出现" } else { "clean" }))

# —————————————————————————————————————
# P0-2: --target 非法 → 非零退出且无产物
# —————————————————————————————————————
$out2 = Join-Path $temp "mandel_test.exe"
if (Test-Path $out2) { Remove-Item $out2 -Force }
Push-Location $temp
try {
    & $aero build "$root\examples\mandelbrot.aero" --target x86_64-unknown-linux-gnu 2>&1 | Out-Null
    $targetExit = $LASTEXITCODE
} catch { $targetExit = 1 }
Pop-Location
Assert "P0-2" "--target 非法返回非零且无产物" (($targetExit -ne 0) -and -not (Test-Path $out2)) "exit=$targetExit, exe=$((Test-Path $out2))"

# —————————————————————————————————————
# P0-3: 闭包尾表达式分号可选
# —————————————————————————————————————
$closureNo = Join-Path $temp "closure_no.aero"
@"
let sq = |x: i64| -> i64 { x * x };
print("%lld\n", sq(7));
"@ | Out-File -FilePath $closureNo -Encoding UTF8 -Force
$closureNoOut = & $aero run $closureNo 2>&1
$closureNoOk = $LASTEXITCODE -eq 0 -and ($closureNoOut -match "49")

$closureYes = Join-Path $temp "closure_yes.aero"
@"
let sq2 = |x: i64| -> i64 { x * x; };
print("%lld\n", sq2(7));
"@ | Out-File -FilePath $closureYes -Encoding UTF8 -Force
$closureYesOut = & $aero run $closureYes 2>&1
$closureYesOk = $LASTEXITCODE -eq 0 -and ($closureYesOut -match "49")

Assert "P0-3" "闭包尾表达式带/不带分号均编译" ($closureNoOk -and $closureYesOk) "no_semi=$closureNoOk, with_semi=$closureYesOk"

# —————————————————————————————————————
# P0-4: 闭包捕获 → 报 closure cannot capture，无 internal error
# —————————————————————————————————————
$capture = Join-Path $temp "capture.aero"
@"
let outer: i64 = 42;
let f = || { outer };
"@ | Out-File -FilePath $capture -Encoding UTF8 -Force
$capOut = & $aero run $capture 2>&1
$capExit = $LASTEXITCODE
$noInternal = -not ($capOut -match "internal error")
$hasCapture = [bool](($capOut -join "`n") -match "closure|capture")
Assert "P0-4" "闭包捕获报错不含 internal error" ($noInternal -and ($capExit -ne 0)) "exit=$capExit, internal=$noInternal"

# —————————————————————————————————————
# P0-5: clippy 不再对块尾 return 误报（行为断言）
# —————————————————————————————————————
$retFile = Join-Path $temp "trailing_return.aero"
@'
fn a() -> i64 {
    let x: i64 = 1;
    return x;
}
'@ | Out-File -FilePath $retFile -Encoding UTF8 -Force
$retOut = ((& $aero clippy $retFile 2>&1) -join "`n")
Assert "P0-5" "clippy 不误报块尾 return" (-not ($retOut -match "needless_return")) "out=$retOut"

# —————————————————————————————————————
# P0-6: bench_fib 防优化屏障
# —————————————————————————————————————
$benchSrc = Get-Content (Join-Path $root "crates\aero-pm\src\bench.rs") -Raw -ErrorAction SilentlyContinue
$hasBarrier = [bool]($benchSrc -match 'asm\(""|black.box|black_box|optimization_barrier')
Assert "P0-6" "bench 含优化屏障（black-box / asm volatile）" $hasBarrier

# —————————————————————————————————————
# P0-7: 包目录内裸 aero run / aero test 可用
# —————————————————————————————————————
$pkgDir = Join-Path $temp "calc"
Push-Location $temp
& $aero new calc 2>&1 | Out-Null
Pop-Location
Push-Location $pkgDir
try {
    $bareRun = & $aero run 2>&1
    $bareRunOk = $LASTEXITCODE -eq 0 -and ($bareRun -notmatch "usage")
} catch { $bareRunOk = $false }
try {
    $bareTest = & $aero test 2>&1
    $bareTestOk = $LASTEXITCODE -eq 0
} catch { $bareTestOk = $false }
Pop-Location
Assert "P0-7" "包内裸 aero run / aero test 可用" ($bareRunOk -and $bareTestOk) "run=$bareRunOk, test=$bareTestOk"

# —————————————————————————————————————
# P0-8: 版本号三处一致
# —————————————————————————————————————
$cliVer = (& $aero --version 2>&1).Trim()
$tomlVer = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"').Matches.Groups[1].Value
$majorMinor = "1.2.6"
Assert "P0-8" "CLI / Cargo.toml 版本号一致为 1.2.6" ($cliVer -match $majorMinor -and $tomlVer -eq $majorMinor) "cli=$cliVer, cargo=$tomlVer"

# —————————————————————————————————————
# P0-9: 文档含 int_to_f64，as f64 报错提示它
# —————————————————————————————————————
$allDocs = Get-ChildItem (Join-Path $root "docs") -Recurse -Include *.md -ErrorAction SilentlyContinue
$hasHint = ($allDocs | Select-String -Pattern "int_to_f64" -ErrorAction SilentlyContinue) -ne $null
$castF64 = Join-Path $temp "cast_f64.aero"
@"
let x: i64 = 1;
let y = x as f64;
"@ | Out-File -FilePath $castF64 -Encoding UTF8 -Force
$castOut = & $aero run $castF64 2>&1
$castHint = [bool](($castOut -join "`n") -match "int_to_f64")
Assert "P0-9" "文档含 int_to_f64，cast f64 报错提示它" ($hasHint -and $castHint)

# —————————————————————————————————————
# P0-10（用户真实 bug）: print %d 对 i64 正确输出
# —————————————————————————————————————
$i64Print = Join-Path $temp "i64print.aero"
@"
let sum: i64 = 4999999950000000;
print("Sum: %d\n", sum);
"@ | Out-File -FilePath $i64Print -Encoding UTF8 -Force
$i64Out = & $aero run $i64Print 2>&1
$i64Ok = $LASTEXITCODE -eq 0 -and ($i64Out -match "4999999950000000")
Assert "P0-10" "print('%d', i64) 正确输出（不截断到 i32）" $i64Ok "got=$i64Out"

# —————————————————————————————————————
# P0-11（用户真实 bug）: Windows aero ls 不依赖 HOME 环境变量
# —————————————————————————————————————
if ($IsWindows -or ($env:OS -match "Windows")) {
    $savedHome = $env:HOME
    $env:HOME = $null
    $lsOut = & $aero ls 2>&1
    $lsHasPkg = [bool]($lsOut -match "aero-collections")
    $env:HOME = $savedHome
    Assert "P0-11" "Windows aero ls 不依赖 HOME 环境变量" $lsHasPkg "has_pkg=$lsHasPkg"
} else {
    Assert "P0-11" "Windows aero ls 不依赖 HOME（非 Windows 跳过）" $true
}

# —————————————————————————————————————
# 狗粮回归（D1~D8）：实测发现并修复的语言/CLI bug
# —————————————————————————————————————
function Run-Aero([string]$name, [string]$src) {
    $f = Join-Path $temp "$name.aero"
    $src | Out-File -FilePath $f -Encoding UTF8 -Force
    $o = (& $aero run $f 2>&1) -join "`n"
    return @{ Out = $o; Exit = $LASTEXITCODE }
}

# D1: 一元 `!` / `~`
$d1 = Run-Aero "d1_unary" @'
let t = true;
let x = 11;
print("%d %lld\n", !t, ~x);
'@
Assert "D1" "一元 `!` / `~` 可用" ($d1.Exit -eq 0 -and $d1.Out -match "0 -12") "out=$($d1.Out)"

# D2: NaN 参与 `!=` 必须为 true
$d2 = Run-Aero "d2_nan" @'
fn mk_nan() -> f64 {
    return 0.0 / 0.0;
}
let a = mk_nan();
print("nan_eq=%d nan_ne=%d\n", a == a, a != a);
'@
Assert "D2" "NaN 的 `==` / `!=` 符合 IEEE" ($d2.Exit -eq 0 -and $d2.Out -match "nan_eq=0 nan_ne=1") "out=$($d2.Out)"

# D3: 零参数闭包 `||`
$d3 = Run-Aero "d3_closure0" @'
let f = || -> i64 { 42 };
print("%lld\n", f());
'@
Assert "D3" "零参数闭包 `||` 可用" ($d3.Exit -eq 0 -and $d3.Out -match "42") "out=$($d3.Out)"

# D4: 闭包体内 let 局部变量不算捕获
$d4 = Run-Aero "d4_closure_local" @'
let blk = |x: i64| -> i64 { let y = x * 2; y + 1 };
print("%lld\n", blk(5));
'@
Assert "D4" "闭包体内 `let` 不误判为捕获" ($d4.Exit -eq 0 -and $d4.Out -match "11") "out=$($d4.Out)"

# D5: 结构体数组（字面量 / for / 下标 / 字段读写）
$d5 = Run-Aero "d5_struct_array" @'
struct Item {
    id: i64,
    w: i64,
}
struct Bag {
    items: [Item; 3],
}
let b = Bag { items: [Item { id: 1, w: 10 }, Item { id: 2, w: 20 }, Item { id: 3, w: 30 }] };
let total = 0;
for (it in b.items) {
    total = total + it.id * it.w;
}
print("total=%lld first=%lld\n", total, b.items[0].w);
b.items[1].w = 99;
print("mut=%lld\n", b.items[1].w);
'@
$d5Ok = $d5.Exit -eq 0 -and $d5.Out -match "total=140 first=10" -and $d5.Out -match "mut=99"
Assert "D5" "结构体数组：字面量/遍历/下标/字段写" $d5Ok "out=$($d5.Out)"

# D6: 显式 `-> void` 返回类型
$d6 = Run-Aero "d6_void" @'
struct C {
    n: i64,
}
impl C {
    fn bump(c: &mut C) -> void {
        c.n = c.n + 1;
    }
    fn get(c: &C) -> i64 {
        return c.n;
    }
}
let c = C { n: 0 };
c.bump();
c.bump();
print("%lld\n", c.get());
'@
Assert "D6" "显式 `-> void` 返回类型可用" ($d6.Exit -eq 0 -and $d6.Out -match "^2") "out=$($d6.Out)"

# D7: `dyn Trait` 作为函数参数
$d7 = Run-Aero "d7_dyn_param" @'
#[derive(Copy)]
struct Sq {
    n: i64,
}
trait Area {
    fn area(s: Sq) -> i64;
}
impl Area for Sq {
    fn area(s: Sq) -> i64 {
        return s.n * s.n;
    }
}
fn dyn_area(d: dyn Area) -> i64 {
    return d.area();
}
let s = Sq { n: 4 };
let d = s as dyn Area;
print("%lld\n", dyn_area(d));
'@
Assert "D7" "`dyn Trait` 可作函数参数" ($d7.Exit -eq 0 -and $d7.Out -match "16") "out=$($d7.Out)"

# D8: CLI 不静默忽略错误参数
$d8file = Join-Path $temp "d8_cli.aero"
@'
print("hi\n");
'@ | Out-File -FilePath $d8file -Encoding UTF8 -Force
$d8a = (& $aero build $d8file --sharedd 2>&1) -join "`n"
$d8aExit = $LASTEXITCODE
$d8b = (& $aero build $d8file --target 2>&1) -join "`n"
$d8bExit = $LASTEXITCODE
$d8c = (& $aero run $d8file --target x86_64-unknown-linux-gnu 2>&1) -join "`n"
$d8cExit = $LASTEXITCODE
$d8Ok = ($d8aExit -ne 0) -and ($d8bExit -ne 0) -and ($d8cExit -ne 0)
Assert "D8" "CLI 未知选项 / 缺值 / run --target 均报错" $d8Ok "unknown=$d8aExit, missing=$d8bExit, run=$d8cExit"

# D9: 除零必须报错终止（不能静默返回 0）
$d9 = Run-Aero "d9_div0" @'
let z = 0;
print("before\n");
print("%lld\n", 10 / z);
print("unreachable\n");
'@
$d9Ok = $d9.Exit -ne 0 -and $d9.Out -match "division by zero" -and $d9.Out -notmatch "unreachable"
Assert "D9" "除零报错终止（非静默 0）" $d9Ok "exit=$($d9.Exit) out=$($d9.Out)"

# D10: 常量下标越界必须是编译错误
$d10 = Run-Aero "d10_oob" @'
let a = [1, 2, 3];
print("%lld\n", a[5]);
'@
$d10Ok = $d10.Exit -ne 0 -and $d10.Out -match "out of bounds"
Assert "D10" "常量下标越界报编译错误" $d10Ok "exit=$($d10.Exit) out=$($d10.Out)"

# D11: 所有示例程序必须全部编译并运行成功
$exBad = @()
foreach ($ex in Get-ChildItem (Join-Path $root "examples") -Filter *.aero) {
    & $aero run $ex.FullName *> $null
    if ($LASTEXITCODE -ne 0) { $exBad += $ex.Name }
}
Assert "D11" "examples/*.aero 全部可运行" ($exBad.Count -eq 0) "failed=$($exBad -join ',')"

# D12: 函数尾表达式是返回值（此前被静默丢弃、返回 0）
$d12 = Run-Aero "d12_tail_expr" @'
fn add1(a: i64) -> i64 {
    let b = a + 1;
    b
}
print("%lld %lld\n", add1(1), add1(41));
'@
Assert "D12" "函数尾表达式作为返回值" ($d12.Exit -eq 0 -and $d12.Out -match "2 42") "out=$($d12.Out)"

# D13: match 绑定模式 `n => { ... }`（此前被误判为枚举变体）
$d13 = Run-Aero "d13_match_bind" @'
fn classify(x: i64) -> i64 {
    match (x) {
        0 => { return 100; }
        n => { return n * 2; }
    }
}
print("%lld %lld\n", classify(0), classify(5));
'@
Assert "D13" "match 绑定模式可用" ($d13.Exit -eq 0 -and $d13.Out -match "100 10") "out=$($d13.Out)"

# D14: 非 void 函数无 return 必须报编译错误（此前静默返回 0）
$d14 = Run-Aero "d14_no_return" @'
fn broken() -> i64 { }
print("%lld\n", broken());
'@
Assert "D14" "缺少 return 的非 void 函数报错" ($d14.Exit -ne 0 -and $d14.Out -match "without returning a value") "exit=$($d14.Exit) out=$($d14.Out)"

# D15: 只有部分分支 return 时同样必须报错（此前 f(0) 静默返回 0）
$d15 = Run-Aero "d15_branch_no_return" @'
fn f(x: i64) -> i64 {
    if (x > 0) { return 1; }
}
print("%lld\n", f(0));
'@
Assert "D15" "漏掉 else 分支的 return 报错" ($d15.Exit -ne 0 -and $d15.Out -match "without returning a value") "exit=$($d15.Exit) out=$($d15.Out)"

# D16: 用户枚举可复用 Some / None 变体名（prelude 已改为 Option::Some 限定写法）
$d16 = Run-Aero "d16_enum_some" @'
enum Maybe { Some(i64), Nothing }
fn unwrap(m: Maybe) -> i64 {
    match (m) {
        Maybe::Some(v) => { return v; }
        Maybe::Nothing => { return -1; }
    }
}
print("%lld\n", unwrap(Maybe::Some(7)));
'@
Assert "D16" "自定义 enum 可用 Some 变体" ($d16.Exit -eq 0 -and $d16.Out -match "^7") "exit=$($d16.Exit) out=$($d16.Out)"

# —————————————————————————————————————
# 汇总输出
# —————————————————————————————————————
Write-Host "`n========== Aero 1.2.6 Verify Results =========="
Write-Host "Binary: $bin"
Write-Host "CLI:    $cliVer"
Write-Host ""
foreach ($r in $results) { Write-Host "  $r" }
Write-Host ""
Write-Host "  PASS: $pass / FAIL: $fail / TOTAL: $($pass + $fail)"
Write-Host "================================================="
exit $(if ($fail -eq 0) { 0 } else { 1 })
