# Runs a title under xb1layer from the title's own folder, with a timeout, and prints the
# exit code plus whatever the layer wrote to stderr.
#
#   .\tools\run-title.ps1 -Title "C:\path\to\title.exe" -Seconds 20 -Ring
param(
    [Parameter(Mandatory)][string]$Title,
    [int]$Seconds = 20,
    [switch]$Ring,                       # also write a crash-proof trace to trace.ring
    [string[]]$ExtraArgs = @(),
    [int]$Tail = 0                       # when > 0, only show the last N stderr lines
)

$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "target\debug\xb1layer.exe"
$dir = Split-Path -Parent $Title
$argList = @('"' + $Title + '"', '--run') + $ExtraArgs
if ($Ring) { $argList += "--ring=" + (Join-Path $root "trace.ring") }

$psi = New-Object Diagnostics.ProcessStartInfo $exe
$psi.WorkingDirectory = $dir
$psi.Arguments = $argList -join ' '
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$p = [Diagnostics.Process]::Start($psi)
$err = $p.StandardError.ReadToEndAsync()
$out = $p.StandardOutput.ReadToEndAsync()
$done = $p.WaitForExit($Seconds * 1000)
if (-not $done) { $p.Kill(); $p.WaitForExit() }
"exited on its own: $done, exit code: 0x{0:X}" -f $p.ExitCode
$lines = $err.Result -split "`r?`n"
if ($Tail -gt 0) { $lines = $lines | Select-Object -Last $Tail }
$lines
