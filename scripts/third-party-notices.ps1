# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

#Requires -Version 7

<#
.SYNOPSIS
Writes the third-party notices a client ships with its binaries.

.DESCRIPTION
Lists the crates that end up in the given manifests' binaries on the given
targets (normal dependencies only, not build or dev ones) and appends the
license files each crate publishes, with identical texts merged. Run it again
whenever Cargo.lock changes. The output does not depend on the machine.

.EXAMPLE
# From a PowerShell 7 prompt:
../rinflow-core/scripts/third-party-notices.ps1 `
    -Manifests agent/Cargo.toml, input/Cargo.toml `
    -Targets x86_64-pc-windows-msvc, aarch64-pc-windows-msvc `
    -Header installer/third-party-header.txt `
    -Output THIRD_PARTY_NOTICES.txt
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string[]]$Manifests,
    [Parameter(Mandatory)][string[]]$Targets,
    [Parameter(Mandatory)][string]$Header,
    [Parameter(Mandatory)][string]$Output
)

$ErrorActionPreference = 'Stop'
$platforms = $Targets | ForEach-Object { '--filter-platform', $_ }
$crates = @{}

foreach ($manifest in $Manifests) {
    & cargo fetch --quiet --locked --manifest-path $manifest
    if ($LASTEXITCODE) { throw "cargo fetch failed for $manifest" }
    $json = & cargo metadata --format-version 1 --locked --manifest-path $manifest @platforms
    if ($LASTEXITCODE) { throw "cargo metadata failed for $manifest" }
    $metadata = $json | ConvertFrom-Json -Depth 64

    $packages = @{}
    foreach ($package in $metadata.packages) { $packages[$package.id] = $package }
    $nodes = @{}
    foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }

    $seen = [System.Collections.Generic.HashSet[string]]::new()
    $pending = [System.Collections.Generic.Stack[string]]::new()
    $pending.Push($metadata.resolve.root)
    while ($pending.Count) {
        $id = $pending.Pop()
        if (-not $seen.Add($id)) { continue }
        foreach ($dep in $nodes[$id].deps) {
            if ($dep.dep_kinds | Where-Object { $null -eq $_.kind }) { $pending.Push($dep.pkg) }
        }
        # Crates without a source are this project's own.
        if ($packages[$id].source) { $crates[$id] = $packages[$id] }
    }
}

$sorted = $crates.Values | Sort-Object { $_.name }, { $_.version }
$texts = [ordered]@{}
$missing = @()
foreach ($crate in $sorted) {
    $folder = Split-Path $crate.manifest_path
    $files = @(Get-ChildItem -LiteralPath $folder -File |
        Where-Object Name -Match '^(licen[cs]e|copying|notice|unlicense|copyright)')
    $licenses = Join-Path $folder 'LICENSES'
    if (Test-Path $licenses) { $files += Get-ChildItem -LiteralPath $licenses -File }
    if (-not $files) { $missing += $crate; continue }
    foreach ($file in ($files | Sort-Object Name)) {
        $text = (Get-Content -Raw -LiteralPath $file.FullName) -replace "`r`n", "`n"
        $text = $text.Trim()
        if (-not $texts.Contains($text)) { $texts[$text] = [System.Collections.Generic.List[string]]::new() }
        $label = "$($crate.name) $($crate.version)"
        if (-not $texts[$text].Contains($label)) { $texts[$text].Add($label) }
    }
}

$rule = '=' * 78
$out = [System.Text.StringBuilder]::new()
[void]$out.AppendLine(((Get-Content -Raw -LiteralPath $Header) -replace "`r`n", "`n").Trim())
[void]$out.AppendLine()
[void]$out.AppendLine($rule).AppendLine('Rust crates').AppendLine($rule).AppendLine()
foreach ($crate in $sorted) {
    $license = if ($crate.license) { $crate.license } else { 'see the crate' }
    [void]$out.AppendLine("$($crate.name) $($crate.version): $license")
    if ($crate.repository) { [void]$out.AppendLine("  $($crate.repository)") }
}
foreach ($text in $texts.Keys) {
    [void]$out.AppendLine().AppendLine($rule)
    [void]$out.AppendLine('Used by: ' + ($texts[$text] -join ', ')).AppendLine($rule).AppendLine()
    [void]$out.AppendLine($text)
}

$content = $out.ToString() -replace "`r`n", "`n"
[System.IO.File]::WriteAllText((Join-Path (Get-Location) $Output), $content, [System.Text.UTF8Encoding]::new($false))
Write-Host "$Output`: $($sorted.Count) crates, $($texts.Count) distinct license texts"
foreach ($crate in $missing) { Write-Warning "$($crate.name) $($crate.version) publishes no license file ($($crate.license))" }
