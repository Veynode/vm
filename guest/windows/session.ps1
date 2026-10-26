# Runs a PowerShell script in the Windows desktop session of the logged-on
# user, for the daemon's desktop and accessibility tools. SSH sessions have no
# desktop (UI Automation and window APIs see nothing there), so this runs as
# the SSH user's command and starts the script through a one-off scheduled
# task with the console user's interactive token, hidden (conhost --headless:
# no console window appears on the user's desktop), then deletes the task.
#
# stdin: one JSON line (the request: method and arguments), then the script.
# stdout: the script's JSON result, or {"error": ...}.
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
# The SSH console uses the OEM code page; the daemon reads and writes UTF-8.
[Console]::InputEncoding = [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
function Fail($message) { ConvertTo-Json -Compress @{ error = $message }; exit 1 }
$stdin = [Console]::In.ReadToEnd()
$newline = $stdin.IndexOf("`n")
if ($newline -lt 0) { Fail 'session: request line missing' }
$user = (Get-CimInstance Win32_ComputerSystem).UserName
if (-not $user) { Fail 'no user is logged on to the Windows desktop (desktop tools need an interactive session; enable automatic logon or sign in on the console)' }
$id = [guid]::NewGuid().ToString('n')
$root = Join-Path $env:ProgramData 'VeynodeVM\desktop'
$dir = Join-Path $root $id
New-Item -ItemType Directory -Force $dir | Out-Null
$name = "VeynodeVM desktop $id"
try {
    # Only SYSTEM, administrators and the desktop user read the request (it
    # can carry text to type) and write the result.
    icacls.exe $dir /inheritance:r /grant:r 'SYSTEM:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "${user}:(OI)(CI)F" | Out-Null
    $utf8 = New-Object Text.UTF8Encoding $false
    [IO.File]::WriteAllText("$dir\request.json", $stdin.Substring(0, $newline), $utf8)
    [IO.File]::WriteAllText("$dir\run.ps1", $stdin.Substring($newline + 1), (New-Object Text.UTF8Encoding $true))
    $powershell = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
    $xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Principals><Principal id="user"><UserId>$([Security.SecurityElement]::Escape($user))</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT1M</ExecutionTimeLimit><Hidden>true</Hidden><Priority>4</Priority></Settings>
  <Actions Context="user"><Exec><Command>$env:SystemRoot\System32\conhost.exe</Command><Arguments>--headless $powershell -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$dir\run.ps1" "$dir"</Arguments></Exec></Actions>
</Task>
"@
    [IO.File]::WriteAllText("$dir\task.xml", $xml, [Text.Encoding]::Unicode)
    $out = schtasks.exe /create /tn $name /xml "$dir\task.xml" /f 2>&1
    if ($LASTEXITCODE) { Fail "session: creating the task failed: $out" }
    $out = schtasks.exe /run /tn $name 2>&1
    if ($LASTEXITCODE) { Fail "session: starting the task failed: $out" }
    $deadline = (Get-Date).AddSeconds(25)
    while (-not (Test-Path "$dir\done") -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 20 }
    if (-not (Test-Path "$dir\done")) { Fail 'desktop script did not finish within 25 s' }
    [Console]::Out.Write([IO.File]::ReadAllText("$dir\out.json"))
} finally {
    schtasks.exe /end /tn $name 2>&1 | Out-Null
    schtasks.exe /delete /tn $name /f 2>&1 | Out-Null
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
    # Not recursive: only when no other call is running.
    foreach ($empty in $root, (Split-Path $root)) { try { [IO.Directory]::Delete($empty) } catch {} }
}
