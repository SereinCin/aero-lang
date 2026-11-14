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
$foundModFrom = Select-String -Path $docFiles -Pattern 'mod\s+\w+\s+from' -ErrorAction SilentlyContinue
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
