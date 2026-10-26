# vm_file_* for Windows guests, run over the guest's OpenSSH Server. One file
# name in the SSH user's files directory (%USERPROFILE%\veynodevm-files); the
# host stages transfers and checks sizes and SHA-256.
#
# stdin: one JSON line, {"op":"put","name","size","sha256"} followed by the
# bytes, or {"op":"get","name","max"}.
# stdout: put: {"published", "size", "sha256", "path"}; get: a JSON line
# {"size", "path"} followed by the bytes. Failures: {"error", "code"}.
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
# The SSH console uses the OEM code page; the daemon reads and writes UTF-8.
[Console]::InputEncoding = [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
function Fail($code, $message) { [Console]::Out.Write((ConvertTo-Json -Compress @{ code = $code; error = $message })); exit 1 }
$in = [Console]::OpenStandardInput()
$line = New-Object Collections.Generic.List[byte]
while (($b = $in.ReadByte()) -ne -1 -and $b -ne 10) { $line.Add([byte]$b) }
$request = [Text.Encoding]::UTF8.GetString($line.ToArray()) | ConvertFrom-Json
$root = Join-Path $HOME 'veynodevm-files'
$path = Join-Path $root $request.name
if ($request.op -eq 'put') {
    New-Item -ItemType Directory -Force $root | Out-Null
    $part = Join-Path $root ('.veynodevm-' + [guid]::NewGuid().ToString('n') + '.part')
    try {
        $file = [IO.File]::Open($part, 'CreateNew', 'Write', 'None')
        try {
            $buffer = New-Object byte[] 65536
            $left = [long]$request.size
            while ($left -gt 0) {
                $n = $in.Read($buffer, 0, [Math]::Min($buffer.Length, $left))
                if ($n -le 0) { break }
                $file.Write($buffer, 0, $n); $left -= $n
            }
        } finally { $file.Close() }
        if ($left -ne 0) { Fail 'invalid_request' 'upload ended early' }
        $digest = (Get-FileHash -Algorithm SHA256 $part).Hash.ToLower()
        if ($digest -ne $request.sha256) { Fail 'changed' 'SHA256 changed in transit' }
        # Move does not replace an existing file: no overwrite.
        try { [IO.File]::Move($part, $path) } catch [IO.IOException] { Fail 'exists' "$($request.name) already exists in the files directory" }
        [Console]::Out.Write((ConvertTo-Json -Compress ([ordered]@{ published = $true; size = [long]$request.size; sha256 = $digest; path = $path })))
    } finally { Remove-Item -Force $part -ErrorAction SilentlyContinue }
} elseif ($request.op -eq 'get') {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    if (-not $item) { Fail 'not_found' "$($request.name) is not in the files directory ($root)" }
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Fail 'invalid_request' "$($request.name) is not a regular file" }
    if ($item.Length -gt [long]$request.max) { Fail 'too_large' "$($request.name) is larger than $($request.max) bytes" }
    $bytes = [IO.File]::ReadAllBytes($path)
    $out = [Console]::OpenStandardOutput()
    $header = [Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -Compress ([ordered]@{ size = $bytes.Length; path = $path })) + "`n")
    $out.Write($header, 0, $header.Length)
    $out.Write($bytes, 0, $bytes.Length)
    $out.Flush()
} else { Fail 'invalid_request' 'unknown file operation' }
