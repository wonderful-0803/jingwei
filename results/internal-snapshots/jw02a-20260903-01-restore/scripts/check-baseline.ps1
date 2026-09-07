[CmdletBinding()]
param(
    [ValidateRange(1, 32)]
    [int]$Jobs = 4
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$cargoExecutable = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path -LiteralPath $cargoExecutable)) {
    throw 'Cargo is not installed at the standard user location.'
}
if (-not (Test-Path -LiteralPath (Join-Path $repositoryRoot 'Cargo.lock'))) {
    throw 'Run cargo fetch in the repository first to resolve and download the baseline dependencies.'
}
if (-not (Test-Path -LiteralPath (Join-Path $repositoryRoot 'tests\model-protocol\Cargo.lock'))) {
    throw 'Restore/prepare the private model-protocol validation workspace and lockfile first.'
}

# Set PATH only for this process and its children; do not rewrite user settings.
$env:PATH = "$(Split-Path -Parent $cargoExecutable);$env:PATH"
$runDirectory = Join-Path $repositoryRoot ("results\baseline\" + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
New-Item -ItemType Directory -Path $runDirectory | Out-Null
$checks = @(
    @{ Name = 'fmt'; Arguments = @('fmt', '--all', '--', '--check') },
    @{ Name = 'check'; Arguments = @('check', '--workspace', '--all-targets', '--all-features', '--locked', '--offline', '--jobs', "$Jobs") },
    @{ Name = 'test'; Arguments = @('test', '--workspace', '--all-features', '--locked', '--offline', '--jobs', "$Jobs") },
    @{ Name = 'clippy'; Arguments = @('clippy', '--workspace', '--all-targets', '--all-features', '--locked', '--offline', '--jobs', "$Jobs", '--', '-D', 'warnings') },
    @{ Name = 'doc'; Arguments = @('doc', '--workspace', '--all-features', '--no-deps', '--locked', '--offline', '--jobs', "$Jobs") },
    @{ Name = 'private-fmt'; Arguments = @('fmt', '--manifest-path', 'tests/model-protocol/Cargo.toml', '--', '--check') },
    @{ Name = 'private-test'; Arguments = @('test', '--manifest-path', 'tests/model-protocol/Cargo.toml', '--locked', '--offline', '--target-dir', 'target', '--jobs', "$Jobs") },
    @{ Name = 'private-clippy'; Arguments = @('clippy', '--manifest-path', 'tests/model-protocol/Cargo.toml', '--all-targets', '--locked', '--offline', '--target-dir', 'target', '--jobs', "$Jobs", '--', '-D', 'warnings') }
)
$results = @()

Write-Output "Baseline logs: $runDirectory"
foreach ($check in $checks) {
    $stdoutPath = Join-Path $runDirectory "$($check.Name).stdout.log"
    $stderrPath = Join-Path $runDirectory "$($check.Name).stderr.log"
    $startedAt = Get-Date
    Write-Output "Starting: cargo $($check.Arguments -join ' ')"
    $checkProcess = Start-Process -FilePath $cargoExecutable -ArgumentList $check.Arguments `
        -WorkingDirectory $repositoryRoot -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
    $checkProcess.WaitForExit()
    $checkProcess.Refresh()
    $results += [PSCustomObject]@{
        Name = $check.Name
        Command = "cargo $($check.Arguments -join ' ')"
        ExitCode = $checkProcess.ExitCode
        ElapsedSeconds = [math]::Round(((Get-Date) - $startedAt).TotalSeconds, 2)
        Stdout = $stdoutPath
        Stderr = $stderrPath
    }
    Write-Output "Finished $($check.Name): exit=$($checkProcess.ExitCode)"
}

$results | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $runDirectory 'summary.json') -Encoding UTF8
$results | Select-Object Name, ExitCode, ElapsedSeconds | Format-Table -AutoSize
if (@($results | Where-Object { $_.ExitCode -ne 0 }).Count -gt 0) {
    exit 1
}
