# Installs and starts cua-driver, the Guest's computer-use helper
# (docs/computer-use.md), for the daemon's driver link. Runs as the SSH user
# (an administrator). The driver daemon must run in the logged-on user's
# desktop session with that user's elevated token: a scheduled task at logon,
# hidden (conhost --headless, no overlay), which the SSH user's
# `cua-driver mcp --socket` proxy then reaches over \\.\pipe\cua-driver.
# Nothing appears on the user's desktop.
#
# stdin: one JSON line {action: status|prepare|install, sha256}
#   status   whether the pinned binary is installed and the daemon answers
#   prepare  creates the staging directory the daemon copies the binary into
#   install  moves a staged binary in place (checked against sha256), writes
#            the telemetry opt-out, registers the task and starts it
# stdout: one JSON object, or {"error": ...}.
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
function Fail($message) { ConvertTo-Json -Compress @{ error = $message }; exit 1 }
$request = [Console]::In.ReadLine() | ConvertFrom-Json
$dir = Join-Path $env:ProgramFiles 'VeynodeVM\cua'
$exe = Join-Path $dir 'cua-driver.exe'
$staging = Join-Path $env:ProgramData 'VeynodeVM\cua'
$staged = Join-Path $staging 'cua-driver.exe.new'
$task = '\VeynodeVM\cua-driver'
$pipe = 'cua-driver'

function Hash($path) {
    if (-not (Test-Path $path)) { return $null }
    (Get-FileHash -Algorithm SHA256 $path).Hash.ToLowerInvariant()
}
# schtasks.exe with its stderr folded into the output; callers check $LASTEXITCODE.
# Windows PowerShell turns a native command's redirected stderr into a terminating
# error under 'Stop', and schtasks writes there for a task that does not exist yet.
function Schtasks {
    $ErrorActionPreference = 'Continue'
    (& schtasks.exe @args 2>&1 | ForEach-Object { "$_" }) -join ' '
}
function TaskExists { $null = Schtasks /query /tn $task; $LASTEXITCODE -eq 0 }
function Listening { [IO.Directory]::GetFiles('\\.\pipe\') -contains "\\.\pipe\$pipe" }
function DesktopUser {
    $user = (Get-CimInstance Win32_ComputerSystem).UserName
    if (-not $user) { Fail 'no user is logged on to the Windows desktop (cua-driver runs in an interactive session; enable automatic logon or sign in on the console)' }
    $user
}
function Status {
    @{ installed = ((Hash $exe) -eq $request.sha256); running = [bool](Listening); task = (TaskExists) }
}

switch ($request.action) {
    'status' { ConvertTo-Json -Compress (Status) }
    'prepare' {
        New-Item -ItemType Directory -Force $staging | Out-Null
        # Only SYSTEM and administrators write the staged binary.
        icacls.exe $staging /inheritance:r /grant:r 'SYSTEM:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
        Remove-Item -Force $staged -ErrorAction SilentlyContinue
        ConvertTo-Json -Compress @{ staged = $staged }
    }
    'install' {
        $user = DesktopUser
        if (Test-Path $staged) {
            if ((Hash $staged) -ne $request.sha256) { Remove-Item -Force $staged; Fail 'the staged cua-driver.exe does not match the pinned sha256' }
            $null = Schtasks /end /tn $task
            Get-Process cua-driver -ErrorAction SilentlyContinue | Stop-Process -Force
            Start-Sleep -Milliseconds 300
            New-Item -ItemType Directory -Force $dir | Out-Null
            Move-Item -Force $staged $exe
        }
        if ((Hash $exe) -ne $request.sha256) { Fail 'cua-driver.exe is not installed; copy it first' }
        # The desktop user's profile: telemetry off before the first run, and
        # isolated browser profiles that an earlier driver left behind removed.
        $sid = (New-Object Security.Principal.NTAccount $user).Translate([Security.Principal.SecurityIdentifier]).Value
        $profile = (Get-CimInstance Win32_UserProfile | Where-Object SID -eq $sid).LocalPath
        if ($profile) {
            $config = Join-Path $profile '.cua-driver\config.json'
            $settings = @{}
            if (Test-Path $config) {
                try { (Get-Content -Raw $config | ConvertFrom-Json).PSObject.Properties | ForEach-Object { $settings[$_.Name] = $_.Value } } catch {}
            }
            $settings['telemetry_enabled'] = $false
            New-Item -ItemType Directory -Force (Split-Path $config) | Out-Null
            [IO.File]::WriteAllText($config, (ConvertTo-Json -Compress $settings), (New-Object Text.UTF8Encoding $false))
            if (-not (Listening)) {
                Remove-Item -Recurse -Force (Join-Path $profile 'AppData\Local\CuaDriver\BrowserProfiles') -ErrorAction SilentlyContinue
            }
        }
        $quoted = [Security.SecurityElement]::Escape($user)
        $optout = 'set DO_NOT_TRACK=1&amp;set CUA_TELEMETRY=0&amp;set CUA_DRIVER_RS_TELEMETRY_ENABLED=0&amp;set CUA_DRIVER_RS_UPDATE_CHECK=0'
        $xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>$quoted</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="user"><UserId>$quoted</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Hidden>true</Hidden><Priority>4</Priority><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>
  <Actions Context="user"><Exec><Command>$env:SystemRoot\System32\conhost.exe</Command><Arguments>--headless $env:SystemRoot\System32\cmd.exe /d /c "$optout&amp;"$exe" serve --no-overlay"</Arguments></Exec></Actions>
</Task>
"@
        $file = Join-Path $staging 'task.xml'
        New-Item -ItemType Directory -Force $staging | Out-Null
        [IO.File]::WriteAllText($file, $xml, [Text.Encoding]::Unicode)
        $out = Schtasks /create /tn $task /xml $file /f
        Remove-Item -Force $file
        if ($LASTEXITCODE) { Fail "registering the cua-driver task failed: $out" }
        if (-not (Listening)) {
            $out = Schtasks /run /tn $task
            if ($LASTEXITCODE) { Fail "starting the cua-driver task failed: $out" }
            $deadline = (Get-Date).AddSeconds(15)
            while (-not (Listening) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 100 }
            if (-not (Listening)) { Fail 'cua-driver did not open its pipe within 15 s of starting' }
        }
        ConvertTo-Json -Compress (Status)
    }
    default { Fail "unknown action $($request.action)" }
}
