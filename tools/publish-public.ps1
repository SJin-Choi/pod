# Publish the current master tree to the public repository as ONE new commit on
# `main`. Local master keeps the full private history and is never pushed.
#
#   powershell -ExecutionPolicy Bypass -File tools/publish-public.ps1 -Message "..." [-DryRun]
#
# Checks before publishing: no third-party reverse-engineering markers, no
# secrets, no private/ or vendor/conpty files in the tree. The commit uses the
# GitHub noreply identity so no personal or work e-mail becomes public.
param(
  [Parameter(Mandatory = $true)][string]$Message,
  [string]$Source = "master",
  [switch]$DryRun
)
$ErrorActionPreference = "Stop"
Set-Location (Resolve-Path (Join-Path $PSScriptRoot ".."))

$tree = (git rev-parse "$Source^{tree}").Trim()
$parent = (git rev-parse main).Trim()
if ((git rev-parse "$parent^{tree}").Trim() -eq $tree) { Write-Output "main already has this tree; nothing to publish."; exit 0 }

# Blocked patterns live in the git-ignored private/publish-block.txt (one extended
# regex per line) so the list itself, which names private identifiers, is never published.
$patternFile = Join-Path "private" "publish-block.txt"
if (-not (Test-Path $patternFile)) { throw "missing $patternFile (blocked-pattern list); refusing to publish without it" }
$patterns = Get-Content $patternFile | Where-Object { $_ -and -not $_.StartsWith('#') }
$bad = @()
$bad += git ls-tree -r --name-only $tree | Select-String -Pattern '^private/|^vendor/conpty/'
foreach ($p in $patterns) { $bad += git grep -n -I -E $p $tree -- ':!tools/publish-public.ps1' 2>$null }
if ($bad.Count -gt 0) { $bad | Select-Object -First 20 | ForEach-Object { Write-Output "BLOCKED: $_" }; throw "publish blocked: fix the findings above" }

$env:GIT_AUTHOR_NAME = 'SJin-Choi'; $env:GIT_AUTHOR_EMAIL = '168047068+SJin-Choi@users.noreply.github.com'
$env:GIT_COMMITTER_NAME = $env:GIT_AUTHOR_NAME; $env:GIT_COMMITTER_EMAIL = $env:GIT_AUTHOR_EMAIL
$body = "$Message`n`nCo-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
if ($DryRun) { Write-Output "dry run: would commit tree $tree on top of $parent"; git diff --stat $parent $tree | Select-Object -Last 1; exit 0 }
$sha = (git commit-tree $tree -p $parent -m $body).Trim()
git branch -f main $sha
git push origin main
Write-Output "published $sha"
