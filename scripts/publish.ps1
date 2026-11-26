# Commits and pushes the working tree to origin/main (our fork selim-durmus/WinRust, never upstream) with safety checks.
#   publish.ps1                 commit everything that changed, push (message made from the changed files)
#   publish.ps1 -Message "..."  same with your own message
#   publish.ps1 -Auto           used by the Claude Stop hook: silent when there is nothing to do
# Refuses (and unstages) when the change contains binaries, logs, big files or something that looks like a secret.
param([string]$Message, [switch]$Auto)
$ErrorActionPreference = 'Continue' # git writes CRLF warnings to stderr; exit codes are checked by hand
Set-Location (Split-Path $PSScriptRoot)

function Fail($m) { git reset -q 2>&1 | Out-Null; Write-Host "publish: NOT pushed - $m" -ForegroundColor Yellow; exit 1 }

if ((git remote get-url origin) -notmatch 'github\.com[/:]selim-durmus/WinRust') { Fail 'origin is not our fork selim-durmus/WinRust' }
if ((git branch --show-current) -ne 'main') { Fail 'not on main' }
if (-not (git status --porcelain)) { if (-not $Auto) { Write-Host 'publish: nothing to commit' }; exit 0 }

git add -A 2>&1 | Out-Null
$files = @(git diff --cached --name-only)
if (-not $files) { exit 0 }

$bad = $files | Where-Object { $_ -match '\.(exe|dll|pdb|log|reg|pem|key|pfx|env)$' -or $_ -match '(^|/)(target|node_modules)/' }
if ($bad) { Fail ("forbidden files: " + ($bad -join ', ')) }
$big = $files | Where-Object { (Test-Path $_) -and (Get-Item $_).Length -gt 5MB }
if ($big) { Fail ("files over 5 MB: " + ($big -join ', ')) }
$diff = git diff --cached -U0 | Out-String
if ($diff -match 'gh[opsu]_[A-Za-z0-9]{30,}|AKIA[0-9A-Z]{16}|BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY') { Fail 'looks like a secret in the diff' }

if (-not $Message) {
    $short = ($files | Select-Object -First 4) -join ', '
    $Message = "Update: $short" + $(if ($files.Count -gt 4) { " (+$($files.Count - 4) more)" } else { '' })
}
# No Co-Authored-By / assistant attribution (as in the whole history of the fork).
git commit -q -m $Message
git push -q origin main
if ($LASTEXITCODE) { Fail 'git push failed' }
Write-Host "publish: pushed - $Message"
