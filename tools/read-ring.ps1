# Prints the tail of a trace ring file written by `xb1layer --ring=<path>`.
# Layout: u64 total bytes written, then a 64 MB circular text buffer.
param(
    [string]$Path = "trace.ring",
    [int]$Tail = 60,
    [switch]$Collapse   # fold consecutive repeated lines into "line  xN"
)

$size = 64MB
$b = [IO.File]::ReadAllBytes($Path)
$total = [BitConverter]::ToUInt64($b, 0)
if ($total -le $size) {
    $txt = [Text.Encoding]::ASCII.GetString($b, 8, [int]$total)
} else {
    $pos = [int]($total % $size)
    $txt = [Text.Encoding]::ASCII.GetString($b, 8 + $pos, $size - $pos) + [Text.Encoding]::ASCII.GetString($b, 8, $pos)
}
Write-Host "total bytes traced: $total"
$lines = $txt -split "`n" | Where-Object { $_ -ne "" }
if ($Collapse) {
    $out = New-Object System.Collections.Generic.List[string]
    $prev = $null; $n = 0
    foreach ($l in $lines) {
        if ($l -eq $prev) { $n++ } else {
            if ($n -gt 0) { $out[$out.Count - 1] += "  x$($n + 1)" }
            $out.Add($l); $prev = $l; $n = 0
        }
    }
    if ($n -gt 0) { $out[$out.Count - 1] += "  x$($n + 1)" }
    $lines = $out
}
$lines | Select-Object -Last $Tail
