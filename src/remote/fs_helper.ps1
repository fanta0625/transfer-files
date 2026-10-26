$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

function Safe([string]$path, [bool]$missing = $false) {
    $p = [IO.Path]::GetFullPath($path)
    $current = $p
    while ($current) {
        if ([IO.File]::Exists($current) -or [IO.Directory]::Exists($current)) {
            if (([IO.File]::GetAttributes($current) -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Links and junctions are not supported: $current"
            }
        } elseif (-not $missing) { throw "Path does not exist: $current" }
        $parent = [IO.Directory]::GetParent($current)
        if ($null -eq $parent) { break }
        $current = $parent.FullName
    }
    return $p
}
function Digest([string]$path, $limit = $null) {
    $path = Safe $path
    $file = [IO.File]::Open($path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $size = $file.Length
        if ($null -ne $limit) { $size = [long]$limit }
        if ($size -gt $file.Length) { throw 'Resume offset exceeds source size' }
        $remaining = $size
        $buffer = New-Object byte[] 1048576
        while ($remaining -gt 0) {
            $count = $file.Read($buffer, 0, [int][Math]::Min($buffer.Length, $remaining))
            if ($count -eq 0) { throw 'Source changed while hashing' }
            [void]$sha.TransformBlock($buffer, 0, $count, $buffer, 0)
            $remaining -= $count
        }
        [void]$sha.TransformFinalBlock([byte[]]@(), 0, 0)
        return @{size=$size; hash=([BitConverter]::ToString($sha.Hash).Replace('-', '').ToLowerInvariant())}
    } finally { $file.Dispose(); $sha.Dispose() }
}
function Manifest([string]$path) {
    $rows = New-Object 'Collections.Generic.List[object]'
    function Visit([string]$p, [string]$rel) {
        $p = Safe $p
        if ([IO.Directory]::Exists($p)) {
            $rows.Add(@{path=$rel; directory=$true; size=0; hash=''})
            foreach ($item in (Get-ChildItem -LiteralPath $p -Force | Sort-Object Name)) {
                $next = $item.Name
                if ($rel) { $next = $rel + '/' + $next }
                Visit $item.FullName $next
            }
        } else {
            $digest = Digest $p
            $rows.Add(@{path=$rel; directory=$false; size=$digest.size; hash=$digest.hash})
        }
    }
    Visit $path ''
    return ,$rows.ToArray()
}
function NativeArg([string]$value) {
    # CommandLineToArgvW quoting, including trailing backslashes and embedded quotes.
    $value = [regex]::Replace($value, '(\\*)"', '$1$1\"')
    $value = [regex]::Replace($value, '(\\+)$', '$1$1')
    return '"' + $value + '"'
}
function Run($r) {
    switch ($r.op) {
        'list' {
            $p = Safe $r.path
            $rows = New-Object 'Collections.Generic.List[object]'
            foreach ($item in (Get-ChildItem -LiteralPath $p -Force)) {
                $link = ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
                $size = 0
                if (-not $item.PSIsContainer -and -not $link) { $size = $item.Length }
                $rows.Add(@{path=$item.FullName; name=$item.Name; directory=($item.PSIsContainer -and -not $link); link=$link; size=$size; modified=$item.LastWriteTimeUtc.ToString('yyyy-MM-dd HH:mm UTC')})
            }
            return @{path=$p; entries=$rows.ToArray()}
        }
        'exists' { $p = Safe $r.path $true; return ([IO.File]::Exists($p) -or [IO.Directory]::Exists($p)) }
        'manifest' { return ,(Manifest $r.path) }
        'hash' {
            $p = Safe $r.path $true
            if ([IO.File]::Exists($p)) { return Digest $p $r.limit }
            if ([IO.Directory]::Exists($p)) { throw 'Expected a regular file' }
            return $null
        }
        'prepare' {
            $base = $r.base
            if (-not $base) { $base = Join-Path $env:LOCALAPPDATA 'transfer-files' }
            $base = Safe $base $true
            [void][IO.Directory]::CreateDirectory($base)
            $p = Join-Path $base ('.transfer-files-' + $r.id)
            [void](Safe $p $true)
            if (-not [IO.Directory]::Exists($p)) {
                $acl = New-Object Security.AccessControl.DirectorySecurity
                $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
                $acl.SetOwner($sid)
                $acl.SetAccessRuleProtection($true, $false)
                $rule = New-Object Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
                $acl.AddAccessRule($rule)
                [void][IO.Directory]::CreateDirectory($p, $acl)
            }
            $acl = [IO.Directory]::GetAccessControl($p)
            $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier])
            if ($owner -ne [Security.Principal.WindowsIdentity]::GetCurrent().User -or -not $acl.AreAccessRulesProtected) {
                throw 'Staging directory must be private and owned by this user'
            }
            return (Safe $p).Replace('\', '/')
        }
        'mkdir' {
            $p = Safe $r.path $true
            [void][IO.Directory]::CreateDirectory($p)
            [void](Safe $p)
            return $true
        }
        'publish' {
            $src = Safe $r.source
            $dst = Safe $r.target $true
            if ([IO.Directory]::Exists($src)) { [IO.Directory]::Move($src, $dst) }
            else { [IO.File]::Move($src, $dst) }
            return $true
        }
        'remove' {
            $p = Safe $r.path
            if (-not [IO.Path]::GetFileName($p).StartsWith('.transfer-files-')) { throw 'Not a staging directory' }
            [IO.Directory]::Delete($p, $true)
            return $true
        }
        'batch' {
            $batch = [IO.Path]::GetTempFileName()
            try {
                [IO.File]::WriteAllText($batch, $r.batch, (New-Object Text.UTF8Encoding($false)))
                [Console]::WriteLine($r.marker)
                $args = @($r.args) + @('-b', $batch, '--', $r.host)
                $p = New-Object Diagnostics.ProcessStartInfo
                $p.FileName = 'sftp.exe'
                $p.Arguments = (($args | ForEach-Object { NativeArg $_ }) -join ' ')
                $p.UseShellExecute = $false
                $child = [Diagnostics.Process]::Start($p)
                $child.WaitForExit()
                if ($child.ExitCode -ne 0) { throw "SFTP failed with status $($child.ExitCode)" }
            } finally { [IO.File]::Delete($batch) }
            return $true
        }
        default { throw 'Unknown operation' }
    }
}
try {
    $value = Run $request
    $answer = @{ok=$true; value=$value}
} catch { $answer = @{ok=$false; error=$_.Exception.Message} }
$json = ConvertTo-Json -InputObject $answer -Depth 30 -Compress
$encoded = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json))
[Console]::WriteLine('TF_RPC_' + $token + '_BEGIN')
for ($offset = 0; $offset -lt $encoded.Length; $offset += 64) {
    [Console]::WriteLine($encoded.Substring($offset, [Math]::Min(64, $encoded.Length - $offset)))
}
[Console]::WriteLine('TF_RPC_' + $token + '_END')
if (-not $answer.ok) { exit 1 }
