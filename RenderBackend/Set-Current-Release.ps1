param(
    [Parameter(Mandatory=$true)][string]$AppVersion,
    [Parameter(Mandatory=$true)][string]$ReleaseTag,
    [Parameter(Mandatory=$true)][string]$IpaAsset,
    [string]$BaselineRepo = ""
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot
$path = Join-Path $PSScriptRoot "release.json"
if (-not (Test-Path $path)) { throw "Missing $path" }

$current = Get-Content $path -Raw | ConvertFrom-Json
if (-not $BaselineRepo) { $BaselineRepo = [string]$current.baseline_repo }
if ($BaselineRepo -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') { throw "Invalid BaselineRepo: $BaselineRepo" }
if ($ReleaseTag -notmatch '^[A-Za-z0-9._+-]+$') { throw "Invalid ReleaseTag: $ReleaseTag" }
if ($IpaAsset -notmatch '^[^/\\]+\.ipa$') { throw "IpaAsset must be an IPA filename, not a path" }
if ($AppVersion -notmatch '^[A-Za-z0-9._+-]+$') { throw "Invalid AppVersion: $AppVersion" }

$obj = [ordered]@{
    app_version   = $AppVersion
    baseline_repo = $BaselineRepo
    release_tag   = $ReleaseTag
    ipa_asset     = $IpaAsset
}
$json = $obj | ConvertTo-Json
[IO.File]::WriteAllText($path, $json + [Environment]::NewLine, (New-Object Text.UTF8Encoding($false)))

Write-Host "Current Pikmin Pilot release updated:" -ForegroundColor Green
Write-Host "  app_version   = $AppVersion"
Write-Host "  baseline_repo = $BaselineRepo"
Write-Host "  release_tag   = $ReleaseTag"
Write-Host "  ipa_asset     = $IpaAsset"
Write-Host ""
Write-Host "Now commit/push RenderBackend/release.json to the B repo. Render will auto-deploy the new release config." -ForegroundColor Cyan
Write-Host "After Render finishes deploying, open your Render /healthz endpoint and confirm the new app_version."
