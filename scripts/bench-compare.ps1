<#
.SYNOPSIS
Compare camembert's scan speed against known disk-usage tools on Windows.

.DESCRIPTION
The Windows counterpart of scripts/bench-compare.sh (see CLAUDE.md
"Benchmarks"): same deterministic synthetic tree, same competitors where
they exist, same exports under target/bench-results/. Mandatory before and
after any change to the scan hot path.

Three things differ from the bash script, all of them because Windows
differs, not because the script is a lesser port:

- **There is no drop_caches.** -Cold purges the standby list through
  NtSetSystemInformation, which is what RAMMap -Ew does. It needs an
  elevated shell, and it does not evict pages resident in an active
  working set, so a Windows "cold" run is colder than warm but warmer than
  a Linux drop_caches run. The script says so in the export rather than
  letting the number pass for something it is not.
- **A real-time antivirus sits in the filter stack** and taxes every file
  operation the scanners make. The script records Defender's state and any
  exclusion covering the tree, because a run with it on and a run with it
  off are not comparable numbers. It never changes that setting.
- **The competitor set is different.** du/dua/pdu/ncdu are not packaged for
  Windows; robocopy is the native listing baseline and is always present.

.PARAMETER Files
Size of the synthetic tree. Default 200000, matching the bash script so
the two platforms' trees have the same shape.

.PARAMETER Tree
Use an existing directory instead of the synthetic tree. Results are then
machine- and content-specific.

.PARAMETER Cold
Purge the standby list before each run. Requires an elevated shell.

.EXAMPLE
scripts\bench-compare.ps1

.EXAMPLE
scripts\bench-compare.ps1 -Files 50000 -Cold
#>
[CmdletBinding()]
param(
    [int]$Files = 200000,
    [string]$Tree = "",
    [switch]$Cold
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$toolsBin = Join-Path $repo 'target\bench-tools\bin'
$env:PATH = "$toolsBin;$env:PATH"

# --- synthetic tree -------------------------------------------------
# Identical layout to the bash script's: WIDTH top dirs, each with a src/
# fan-out and a node_modules/ subtree, $Files empty files total. Scans are
# metadata-bound, so content would change nothing.
if (-not $Tree) {
    $Tree = Join-Path $repo "target\bench-tree-$Files"
    $stamp = Join-Path $Tree '.complete'
    if (-not (Test-Path $stamp)) {
        $python = (Get-Command python -ErrorAction SilentlyContinue).Source
        if (-not $python) {
            throw "python is needed to generate the synthetic tree; install it or pass -Tree"
        }
        Write-Host "generating synthetic tree ($Files files) in $Tree ..." -ForegroundColor Yellow
        if (Test-Path $Tree) { Remove-Item -Recurse -Force $Tree }
        $gen = Join-Path ([IO.Path]::GetTempPath()) 'camembert-gentree.py'
        @'
import os, sys
root, files = sys.argv[1], int(sys.argv[2])
width = 100
per_dir = 50
made = 0
d = 0
while made < files:
    top = os.path.join(root, f"proj{d % width:03d}")
    for sub in ("src", os.path.join("node_modules", f"dep{d:04d}", "lib")):
        path = os.path.join(top, sub, f"batch{d:05d}")
        os.makedirs(path, exist_ok=True)
        for i in range(per_dir):
            if made >= files:
                break
            open(os.path.join(path, f"f{i:03d}.js" if i % 3 else f"f{i:03d}.log"), "w").close()
            made += 1
    d += 1
open(os.path.join(root, ".complete"), "w").close()
'@ | Out-File -Encoding utf8 $gen
        & $python $gen $Tree $Files
        if ($LASTEXITCODE -ne 0) { throw "tree generation failed" }
    }
    Write-Host "tree: $Tree ($Files files, cached)" -ForegroundColor DarkGray
}
$Tree = (Resolve-Path $Tree).Path

# --- environment, recorded because it changes the numbers -----------
# A benchmark that hides its antivirus is a benchmark that reports a
# different machine's results.
$notes = [System.Collections.Generic.List[string]]::new()
try {
    $mp = Get-MpComputerStatus -ErrorAction Stop
    $rtp = $mp.RealTimeProtectionEnabled
    $excluded = $false
    try {
        $paths = (Get-MpPreference -ErrorAction Stop).ExclusionPath
        if ($paths) {
            $excluded = @($paths | Where-Object { $Tree.StartsWith($_, 'OrdinalIgnoreCase') }).Count -gt 0
        }
    } catch { }
    $notes.Add("Defender real-time protection: $rtp (tree excluded: $excluded)")
} catch {
    $notes.Add("Defender state: unknown (Get-MpComputerStatus unavailable)")
}
$notes.Add("CPU: $((Get-CimInstance Win32_Processor).Name.Trim())")
$notes.Add("OS: $((Get-CimInstance Win32_OperatingSystem).Caption) $([Environment]::OSVersion.Version)")

# --- cold mode ------------------------------------------------------
$prepare = @()
if ($Cold) {
    $isAdmin = ([Security.Principal.WindowsPrincipal] `
        [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $isAdmin) {
        throw "-Cold purges the standby list, which needs an elevated shell. " +
              "Re-run as administrator, or drop -Cold and read the result as a warm-cache run."
    }
    # Empty the standby list: NtSetSystemInformation(SystemMemoryListInformation,
    # MemoryPurgeStandbyList) — the same call RAMMap -Ew makes. Passed as an
    # inline -Command rather than a temp script file, so no execution-policy
    # bypass is involved.
    $purge = @'
Add-Type -Namespace Cam -Name Mm -MemberDefinition '[DllImport("ntdll.dll")] public static extern int NtSetSystemInformation(int c, System.IntPtr i, int l);';
$p = [Runtime.InteropServices.Marshal]::AllocHGlobal(4);
[Runtime.InteropServices.Marshal]::WriteInt32($p, 4);
$rc = [Cam.Mm]::NtSetSystemInformation(80, $p, 4);
[Runtime.InteropServices.Marshal]::FreeHGlobal($p);
if ($rc -ne 0) { Write-Error ('standby purge failed: 0x{0:X8}' -f $rc); exit 1 }
'@ -replace '\r?\n', ' '
    $prepare = @('--prepare', "powershell -NoProfile -Command `"$purge`"")
    $notes.Add("Cold mode: standby list purged before each run (active working sets survive)")
} else {
    $notes.Add("Warm cache")
}

# --- build ----------------------------------------------------------
Write-Host "building camembert --release ..." -ForegroundColor DarkGray
cargo build --release --quiet
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
$camembert = Join-Path $repo 'target\release\camembert.exe'

# --- competitors ----------------------------------------------------
$bench = [System.Collections.Generic.List[object]]::new()
function Add-Bench([string]$name, [string]$cmd) {
    $bench.Add([pscustomobject]@{ Name = $name; Cmd = $cmd })
}
function Have([string]$exe) { [bool](Get-Command $exe -ErrorAction SilentlyContinue) }

Add-Bench 'camembert' "`"$camembert`" --no-ui `"$Tree`""
# robocopy's list-only mode is the native baseline and ships with Windows.
# It reports success with a non-zero exit code, hence hyperfine's -i below.
Add-Bench 'robocopy' "robocopy `"$Tree`" NULL /L /S /NJH /NJS /NC /NS /NFL /NDL"
if (Have 'dust')   { Add-Bench 'dust'   "dust -d 0 `"$Tree`"" }
if (Have 'diskus') { Add-Bench 'diskus' "diskus `"$Tree`"" }
if (Have 'gdu')    { Add-Bench 'gdu'    "gdu -npc `"$Tree`"" }
if (Have 'du')     { Add-Bench 'du'     "du -sb `"$Tree`"" }

Write-Host "competitors: $(($bench.Name) -join ', ')" -ForegroundColor DarkGray
foreach ($n in $notes) { Write-Host "  $n" -ForegroundColor DarkGray }

# --- run ------------------------------------------------------------
$out = Join-Path $repo 'target\bench-results'
New-Item -ItemType Directory -Force -Path $out | Out-Null
$ts = Get-Date -Format 'yyyyMMdd-HHmmss'
$md = Join-Path $out "$ts.md"
$json = Join-Path $out "$ts.json"

if (Have 'hyperfine') {
    # -i: robocopy signals success with a non-zero exit code, and one
    # competitor's convention must not abort the whole comparison.
    $hfArgs = @('--warmup', '2', '--min-runs', '5', '-i') + $prepare +
              @('--export-markdown', $md, '--export-json', $json)
    foreach ($b in $bench) { $hfArgs += @('--command-name', $b.Name, $b.Cmd) }
    # Windows PowerShell 5.1 wraps a native command's stderr in ErrorRecords,
    # so `$ErrorActionPreference = 'Stop'` aborts the run the moment hyperfine
    # emits one of its warnings ("first run significantly slower", ...) — an
    # intermittent failure that silently costs you the whole comparison.
    # Relax the preference across the call only, and check the exit code
    # explicitly instead.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { hyperfine @hfArgs } finally { $ErrorActionPreference = $previous }
    if ($LASTEXITCODE -ne 0) { throw "hyperfine exited with $LASTEXITCODE" }
    if (Test-Path $md) {
        $header = @("<!-- $ts | tree: $Tree ($Files files) -->") +
                  ($notes | ForEach-Object { "<!-- $_ -->" }) + ''
        ($header + (Get-Content $md)) | Set-Content -Encoding utf8 $md
        Copy-Item $md (Join-Path $out 'latest.md') -Force
        Copy-Item $json (Join-Path $out 'latest.json') -Force
        Write-Host "exports: $md (+ latest.*)" -ForegroundColor DarkGray
    }
} else {
    Write-Host "hyperfine not found - falling back to best-of-5 wall clock" -ForegroundColor Yellow
    Write-Host "  (cargo install --locked --root target/bench-tools hyperfine)" -ForegroundColor DarkGray
    foreach ($b in $bench) {
        $best = [double]::MaxValue
        foreach ($i in 1..5) {
            $t = (Measure-Command {
                cmd /c "$($b.Cmd) > NUL 2>&1"
            }).TotalMilliseconds
            if ($t -lt $best) { $best = $t }
        }
        '{0,-12} {1,8:N0} ms (best of 5)' -f $b.Name, $best
    }
}
