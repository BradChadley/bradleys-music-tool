# Writes THIRD-PARTY-NOTICES.txt: the libraries built into the program file, their licenses and the
# license texts they ship with. Run it again after Cargo.lock changes:
#   pwsh -File tools\third-party.ps1
#   pwsh -File tools\third-party.ps1 -Check     (changes nothing; fails when the file is out of date)
# It reads the package list from cargo and the license files from cargo's own copy of each package.
# It goes online only if cargo has not fetched the packages yet.
param([switch]$Check)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root 'THIRD-PARTY-NOTICES.txt'
$target = 'x86_64-pc-windows-msvc'

Push-Location $root
try { $json = cargo metadata --format-version 1 --filter-platform $target --locked 2>$null } finally { Pop-Location }
if ($LASTEXITCODE -ne 0 -or -not $json) { throw 'cargo metadata failed' }
$meta = $json | ConvertFrom-Json -AsHashtable

# The packages that end up in the program file: everything reached from the app through ordinary
# dependencies. What is only needed while building (build scripts and their libraries) is left out.
$nodes = @{}; foreach ($n in $meta.resolve.nodes) { $nodes[$n.id] = $n }
$packages = @{}; foreach ($p in $meta.packages) { $packages[$p.id] = $p }
$seen = [System.Collections.Generic.HashSet[string]]::new()
$queue = [System.Collections.Generic.Queue[string]]::new()
$queue.Enqueue($meta.resolve.root)
while ($queue.Count) {
    $id = $queue.Dequeue()
    if (-not $seen.Add($id)) { continue }
    foreach ($d in $nodes[$id].deps) {
        if ($d.dep_kinds | Where-Object { $null -eq $_.kind }) { $queue.Enqueue($d.pkg) }
    }
}
$app = $packages[$meta.resolve.root]
$crates = @($seen | Where-Object { $_ -ne $meta.resolve.root } | ForEach-Object { $packages[$_] } | Sort-Object { $_.name.ToLowerInvariant() }, { $_.version })

# Each package's license files, and each distinct text once with the packages that carry it.
$sha = [Security.Cryptography.SHA256]::Create()
$texts = [ordered]@{}
$without = [System.Collections.Generic.List[string]]::new()
foreach ($c in $crates) {
    $dir = Split-Path -Parent $c.manifest_path
    $files = @(Get-ChildItem -LiteralPath $dir -File | Where-Object Name -match '^(LICEN[SC]E|COPYING|NOTICE|UNLICENSE|COPYRIGHT)' | Sort-Object Name)
    if (-not $files) { $without.Add("$($c.name) $($c.version)") }
    foreach ($f in $files) {
        $text = ([IO.File]::ReadAllText($f.FullName) -replace "`r`n?", "`n").Trim("`n", ' ', "`t")
        if (-not $text) { continue }
        $lines = $text -split "`n" | ForEach-Object { $_.TrimEnd() }
        $key = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes((($lines -join ' ') -replace '\s+', ' ').ToLowerInvariant())))
        if (-not $texts.Contains($key)) { $texts[$key] = @{ lines = $lines; by = [System.Collections.Generic.List[string]]::new() } }
        $texts[$key].by.Add("$($c.name) $($c.version) ($($f.Name))")
    }
}

$rule = '=' * 100
$thin = '-' * 100
$o = [System.Collections.Generic.List[string]]::new()
$add = { param([string[]]$l) foreach ($x in $l) { $o.Add($x) } }
& $add @(
    "Bradley's Music Tool: what is inside it, and under which licenses"
    $rule
    ''
    "Bradley's Music Tool itself is free software under the GNU General Public License, version 3"
    '(see the file LICENSE). This file lists the work of others that it is built from or works with.'
    'It is written by tools\third-party.ps1; change that script, not this file.'
    ''
    ''
    'INSIDE THE PROGRAM FILE'
    $thin
    ''
    'Fonts (they come with the egui library, in its package epaint_default_fonts)'
    '    Ubuntu Light: Ubuntu Font Licence 1.0, https://design.ubuntu.com/font'
    '    Hack: MIT license, https://github.com/source-foundry/Hack'
    '    Noto Emoji: SIL Open Font License 1.1, https://github.com/googlefonts/noto-emoji'
    '    emoji-icon-font: MIT license, https://github.com/jslegers/emoji-icon-font'
    ''
    "The Rust libraries listed below ($($crates.Count) packages)."
    ''
    ''
    'FETCHED BY THE APP WHEN IT IS ASKED TO, NOT INSIDE IT'
    $thin
    ''
    'FFmpeg (ffmpeg and ffprobe, the "essentials" build by Gyan Doshi)'
    '    Reads, checks and converts music files. GNU General Public License, version 3.'
    '    https://ffmpeg.org   https://github.com/GyanD/codexffmpeg'
    ''
    'slskd'
    '    The Soulseek client the Downloads page drives. GNU Affero General Public License, version 3.'
    '    https://github.com/slskd/slskd'
    ''
    ''
    ''
    'RUST LIBRARIES'
    $thin
    ''
)
$w1 = ($crates | ForEach-Object { $_.name.Length } | Measure-Object -Maximum).Maximum
$w2 = ($crates | ForEach-Object { $_.version.Length } | Measure-Object -Maximum).Maximum
$w3 = ($crates | ForEach-Object { "$($_.license)".Length } | Measure-Object -Maximum).Maximum
foreach ($c in $crates) {
    $where = if ($c.repository) { $c.repository } elseif ($c.homepage) { $c.homepage } else { "https://crates.io/crates/$($c.name)" }
    $o.Add(('{0}  {1}  {2}  {3}' -f $c.name.PadRight($w1), $c.version.PadRight($w2), "$($c.license)".PadRight($w3), $where).TrimEnd())
}
& $add @('', '')
if ($without.Count) {
    & $add @(
        'These packages name their license above and carry no license file of their own; the texts of'
        'those licenses are among the ones below:'
        ''
    )
    $line = '   '
    foreach ($w in $without) {
        if (($line.Length + $w.Length + 2) -gt 98) { $o.Add($line.TrimEnd().TrimEnd(',')+','); $line = '   ' }
        $line += " $w,"
    }
    $o.Add($line.TrimEnd().TrimEnd(','))
    & $add @('', '')
}
& $add @('LICENSE TEXTS', $thin, '', 'Each text is given once, under the packages that ship it.', '')
foreach ($k in $texts.Keys) {
    $t = $texts[$k]
    $o.Add($rule)
    $line = ''
    foreach ($b in $t.by) {
        if ($line -and ($line.Length + $b.Length + 2) -gt 98) { $o.Add($line.TrimEnd().TrimEnd(',') + ','); $line = '' }
        $line += "$b, "
    }
    $o.Add($line.TrimEnd().TrimEnd(','))
    $o.Add($rule)
    $o.Add('')
    foreach ($l in $t.lines) { $o.Add($l) }
    $o.Add('')
}
$new = ($o -join "`r`n") + "`r`n"

if ($Check) {
    $old = if (Test-Path -LiteralPath $out) { [IO.File]::ReadAllText($out) -replace "`r`n?", "`n" } else { '' }
    if ($old -ne ($new -replace "`r`n", "`n")) { Write-Host 'THIRD-PARTY-NOTICES.txt is out of date: run tools\third-party.ps1'; exit 1 }
    Write-Host "THIRD-PARTY-NOTICES.txt is up to date ($($crates.Count) packages)."
    exit 0
}
[IO.File]::WriteAllText($out, $new, [Text.UTF8Encoding]::new($false))
Write-Host "THIRD-PARTY-NOTICES.txt written: $($crates.Count) packages, $($texts.Count) license texts, $([math]::Round($new.Length / 1KB)) KB, $($app.name) $($app.version)."
