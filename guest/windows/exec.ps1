# vm_exec for Windows guests, run over the guest's OpenSSH Server. Starts one
# program directly (no shell: the host passes the program and its command
# line built from argv with the Windows quoting rules), waits up to the
# timeout, then kills its process tree.
#
# stdin: {"file": ..., "arguments": ..., "timeout_ms": ...}
# stdout: {"exit_code", "stdout", "stderr" (base64), "*_truncated", "timed_out", "duration_ms"}
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
# The SSH console uses the OEM code page; the daemon reads and writes UTF-8.
[Console]::InputEncoding = [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
$limit = 65536
$request = [Console]::In.ReadToEnd() | ConvertFrom-Json
$info = New-Object Diagnostics.ProcessStartInfo
$info.FileName = $request.file
$info.Arguments = $request.arguments
$info.WorkingDirectory = $HOME
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
$info.RedirectStandardInput = $true
$info.RedirectStandardOutput = $true
$info.RedirectStandardError = $true
$started = Get-Date
try {
    $process = [Diagnostics.Process]::Start($info)
} catch {
    ConvertTo-Json -Compress @{ error = "cannot start $($request.file): $($_.Exception.InnerException.Message)" }
    exit 0
}
$process.StandardInput.Close()
$out = New-Object IO.MemoryStream; $err = New-Object IO.MemoryStream
$reads = $process.StandardOutput.BaseStream.CopyToAsync($out), $process.StandardError.BaseStream.CopyToAsync($err)
$timedOut = -not $process.WaitForExit([int]$request.timeout_ms)
if ($timedOut) {
    taskkill.exe /T /F /PID $process.Id 2>&1 | Out-Null
    [void]$process.WaitForExit(5000)
}
[void][Threading.Tasks.Task]::WaitAll($reads, 5000)
function Bounded($stream) {
    $bytes = $stream.ToArray()
    if ($bytes.Length -gt $limit) { $bytes = $bytes[0..($limit - 1)] }
    [Convert]::ToBase64String([byte[]]$bytes)
}
$code = if ($timedOut -or -not $process.HasExited) { -1 } else { $process.ExitCode }
ConvertTo-Json -Compress ([ordered]@{
    exit_code = $code
    stdout = Bounded $out; stderr = Bounded $err
    stdout_truncated = $out.Length -gt $limit; stderr_truncated = $err.Length -gt $limit
    timed_out = $timedOut; cancelled = $false
    duration_ms = [int]((Get-Date) - $started).TotalMilliseconds
})
