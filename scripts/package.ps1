<#
.SYNOPSIS
Builds verified Windows x64 editions, portable ZIPs, and optional installers.
.EXAMPLE
./scripts/package.ps1 -WorkDir C:\Build\ZSDesk -TargetDir C:\Build\ZSDesk-target -InnoCompiler 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe'
#>
param(
    [ValidateSet('standard','network','desktop','full')][string[]]$Editions = @('standard','network','desktop','full'),
    [string]$WorkDir = (Join-Path ([IO.Path]::GetTempPath()) 'ZSDesk-build'),
    [string]$TargetDir = $env:CARGO_TARGET_DIR,
    [string]$OutputDir,
    [string]$RoutingDir,
    [string]$RuntimeDir,
    [string]$InnoCompiler,
    [switch]$SkipBuild,
    [string]$BinaryPath
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$WorkDir = [IO.Path]::GetFullPath($WorkDir)
if (-not $TargetDir) { $TargetDir = Join-Path $WorkDir 'target' }
$TargetDir = [IO.Path]::GetFullPath($TargetDir)
$version = [regex]::Match((Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw), '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if ($version -notmatch '^\d+\.\d+\.\d+([+-][A-Za-z0-9.-]+)?$') { throw 'Workspace version is invalid' }
if (-not $OutputDir) { $OutputDir = Join-Path $root "releases\$version" }
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
if ($SkipBuild -and ($Editions.Count -ne 1 -or -not $BinaryPath)) { throw '-SkipBuild requires one edition and an explicit -BinaryPath' }
if ((New-Object IO.DriveInfo ([IO.Path]::GetPathRoot($TargetDir))).AvailableFreeSpace -lt 10GB) { throw 'Less than 10 GiB available on the build volume' }
$definitions = @{
    standard = @{ Name='标准版'; Features=@(); Network=$false; Desktop=$false }
    network = @{ Name='网络版'; Features=@('network'); Network=$true; Desktop=$false }
    desktop = @{ Name='桌面版'; Features=@('desktop'); Network=$false; Desktop=$true }
    full = @{ Name='全量版'; Features=@('network','desktop'); Network=$true; Desktop=$true }
}
if (-not $RuntimeDir) {
    $candidates=@()
    if ($env:VCToolsRedistDir) { $candidates+=Join-Path $env:VCToolsRedistDir 'x64' }
    $vswhere=Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhere) {
        $installation=& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($installation) {
            $redistRoot=Join-Path $installation 'VC\Redist\MSVC'
            if (Test-Path -LiteralPath $redistRoot) {
                $candidates+=@(Get-ChildItem -LiteralPath $redistRoot -Directory | Where-Object { $_.Name -match '^\d+\.\d+\.\d+$' } | Sort-Object { [version]$_.Name } -Descending | ForEach-Object { Join-Path $_.FullName 'x64' })
            }
        }
    }
    foreach ($candidate in $candidates) {
        if (-not (Test-Path -LiteralPath $candidate)) { continue }
        $directory=Get-ChildItem -LiteralPath $candidate -Directory -Filter 'Microsoft.VC*.CRT' | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'msvcp140.dll') } | Select-Object -First 1
        if ($directory) { $RuntimeDir=$directory.FullName; break }
    }
}
if (-not $RuntimeDir) { throw 'Visual C++ x64 Redistributable files are required; supply -RuntimeDir from the Visual Studio Redist directory' }
$RuntimeDir=[IO.Path]::GetFullPath($RuntimeDir)
$runtimeFiles=@('msvcp140.dll','vcruntime140.dll','vcruntime140_1.dll')
foreach ($name in $runtimeFiles) { if (-not (Test-Path -LiteralPath (Join-Path $RuntimeDir $name) -PathType Leaf)) { throw "Redistributable file missing: $name" } }
foreach ($path in @($WorkDir,$TargetDir,$OutputDir)) { [IO.Directory]::CreateDirectory($path) | Out-Null }
$lock = [IO.File]::Open((Join-Path $TargetDir 'zsdesk-package.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
$env:CARGO_TARGET_DIR=$TargetDir
$env:CARGO_INCREMENTAL='0'
$env:CARGO_PROFILE_DEV_DEBUG='0'
$env:CARGO_PROFILE_TEST_DEBUG='0'
$env:CARGO_PROFILE_RELEASE_DEBUG='0'
$zsui=Join-Path (Split-Path -Parent $root) 'zsui'
$env:CARGO_ENCODED_RUSTFLAGS=@("--remap-path-prefix=$root=/src/zsdesk","--remap-path-prefix=$zsui=/src/zsui","--remap-path-prefix=$env:USERPROFILE=/user","--remap-path-prefix=$TargetDir=/target",'-C','link-arg=/PDBALTPATH:zsdesk.pdb') -join [char]31
$env:TEMP=Join-Path $WorkDir 'temp'
$env:TMP=$env:TEMP
[IO.Directory]::CreateDirectory($env:TEMP) | Out-Null

function Get-BuildInfo([string]$Path) {
    if ((Get-Item -LiteralPath $Path).VersionInfo.ProductVersion -ne $version) { throw 'The executable resource version differs from the workspace version' }
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName=$Path; $start.Arguments='--build-info'; $start.UseShellExecute=$false
    $start.CreateNoWindow=$true; $start.WindowStyle=[Diagnostics.ProcessWindowStyle]::Hidden
    $start.RedirectStandardOutput=$true; $start.RedirectStandardError=$true
    $process = [Diagnostics.Process]::Start($start)
    try {
        $output=$process.StandardOutput.ReadToEndAsync(); $errorOutput=$process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(10000)) { $process.Kill(); $process.WaitForExit(); throw 'The owned build identity query timed out' }
        if ($process.ExitCode -ne 0) { throw "Build identity query failed: $($errorOutput.GetAwaiter().GetResult())" }
        return ($output.GetAwaiter().GetResult() | ConvertFrom-Json)
    } finally { $process.Dispose() }
}

function Write-Json([string]$Path,$Value) {
    $Value | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $Path -Encoding utf8
}

function Assert-ChildPath([string]$Path,[string]$Parent) {
    $absolute=[IO.Path]::GetFullPath($Path)
    $boundary=[IO.Path]::GetFullPath($Parent).TrimEnd('\')+'\'
    if (-not $absolute.StartsWith($boundary,[StringComparison]::OrdinalIgnoreCase)) { throw "Path is outside its declared root: $absolute" }
    if (Test-Path -LiteralPath $absolute) {
        if ((Get-Item -LiteralPath $absolute).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse points are not package targets: $absolute" }
    }
}

function Assert-Payload([string]$Directory,$Manifest) {
    $actual = @(Get-ChildItem -LiteralPath $Directory -Recurse -File)
    if ($actual.Count -ne $Manifest.files.Count+1) { throw 'Unexpected files in package payload' }
    foreach ($entry in $Manifest.files.GetEnumerator()) {
        $file=Join-Path $Directory $entry.Key
        if ((Get-Item -LiteralPath $file).Length -ne $entry.Value.bytes -or (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.Value.sha256) { throw "Payload hash mismatch: $($entry.Key)" }
    }
}

try {
    foreach ($edition in $Editions) {
        $definition=$definitions[$edition]
        $name="ZSDesk-$version-$edition-x64"
        $stage=Join-Path $WorkDir $name
        $destination=Join-Path $OutputDir $name
        Assert-ChildPath $stage $WorkDir
        Assert-ChildPath $destination $OutputDir
        if ((Test-Path -LiteralPath $stage) -or (Test-Path -LiteralPath $destination) -or (Test-Path -LiteralPath "$destination.zip")) { throw "Existing package retained; inspect it before retrying: $name" }
        $flags=@('build','--release','--locked','-p','zsdesk-app','--manifest-path',(Join-Path $root 'Cargo.toml'))
        if ($edition -ne 'full') {
            $flags += '--no-default-features'
            if ($definition.Features.Count) { $flags += @('--features',($definition.Features -join ',')) }
        }
        $state=[ordered]@{project='ZSDesk';version=$version;edition=$edition;source=$root;target=$TargetDir;profile='release';features=$definition.Features;runtime_source=$RuntimeDir;pid=$PID;state='running';started=[DateTimeOffset]::Now.ToString('o')}
        Write-Json (Join-Path $WorkDir 'package-state.current.json') $state
        if (-not $SkipBuild) {
            & cargo @flags --message-format=json-render-diagnostics 1>(Join-Path $WorkDir "cargo.$edition.current.jsonl")
            if ($LASTEXITCODE -ne 0) { throw "Cargo failed for $edition" }
            $exe=Join-Path $TargetDir 'release\zsdesk-app.exe'
        } else { $exe=[IO.Path]::GetFullPath($BinaryPath) }
        $identity=Get-BuildInfo $exe
        if ($identity.product -ne 'ZSDesk' -or $identity.version -ne $version -or $identity.edition -ne $edition -or $identity.network -ne $definition.Network -or $identity.desktop -ne $definition.Desktop) { throw "Executable edition/version mismatch: $edition" }
        [IO.Directory]::CreateDirectory($stage) | Out-Null
        Copy-Item -LiteralPath $exe -Destination (Join-Path $stage 'ZSDesk.exe')
        foreach ($runtime in $runtimeFiles) {
            Copy-Item -LiteralPath (Join-Path $RuntimeDir $runtime) -Destination (Join-Path $stage $runtime)
        }
        Copy-Item -LiteralPath (Join-Path $root 'packaging\licenses') -Destination (Join-Path $stage 'licenses') -Recurse
        Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $stage 'LICENSE.txt')
        if ($RoutingDir) {
            $routingFiles=@('ShellRoutingControl.exe','ZSDeskRevealRouter.dll','manifest.json','MinHook-LICENSE.txt')
            [IO.Directory]::CreateDirectory((Join-Path $stage 'routing')) | Out-Null
            foreach ($file in $routingFiles) { Copy-Item -LiteralPath (Join-Path $RoutingDir $file) -Destination (Join-Path $stage "routing\$file") }
        }
        $readme=@("ZSDesk $version $($definition.Name) — Windows x64",'',
            '运行 ZSDesk.exe 打开文件管理器；--classic-menu 使用原版菜单。',
            'Ctrl+F 搜索文件名；Alt+P 切换预览窗格；空格打开快速预览。',
            '≡ → 界面设置：命令栏显示项目、搜索范围和预览选项。',
            '≡ → 右键菜单设置：选择菜单样式和显示功能。',
            '≡ → 系统集成：文件夹默认打开方式。')
        if ($RoutingDir) { $readme += '≡ → 系统集成 → 文件位置转发：选择应用并接入文件定位。' }
        if ($definition.Desktop) { $readme += @('','桌面格子：ZSDesk.exe --desktop，或从主菜单启动。','支持文件夹映射、腾讯布局导入、悬停展开、分组和屏幕布局。') }
        if ($definition.Network) { $readme += @('','支持 SMB/UNC、系统 FTP；SFTP/SSH 地址需要 SSHFS-Win/WinFsp。') }
        $readme += @('','配置默认保存在用户 AppData\Roaming\ZSDesk。','portable.flag 使用程序旁 data 目录；--profile-dir 指定独立配置。','安装包与便携包不包含用户路径、桌面布局、搜索索引或系统接管记录。','源码与许可证：https://github.com/qiu7824/zsdesk')
        $readme -join "`r`n" | Set-Content -LiteralPath (Join-Path $stage '说明.txt') -Encoding utf8BOM
        $files=[ordered]@{}
        foreach ($file in (Get-ChildItem -LiteralPath $stage -File -Recurse | Sort-Object FullName)) {
            if ($file.LastWriteTimeUtc.Year -lt 1980) { $file.LastWriteTimeUtc=[datetime]'1980-01-01T00:00:00Z' }
            $relative=$file.FullName.Substring($stage.Length+1).Replace('\','/')
            $files[$relative]=[ordered]@{bytes=$file.Length;sha256=(Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()}
        }
        $manifest=[ordered]@{product='ZSDesk';version=$version;edition=$edition;edition_name=$definition.Name;platform='windows-x64';features=$definition.Features;files=$files}
        Write-Json (Join-Path $stage 'manifest.json') $manifest
        Assert-Payload $stage $manifest
        $zip=Join-Path $WorkDir "$name.zip"
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [IO.Compression.ZipFile]::CreateFromDirectory($stage,$zip,[IO.Compression.CompressionLevel]::Optimal,$false)
        $archive=[IO.Compression.ZipFile]::OpenRead($zip)
        try { if ($archive.Entries.Count -ne $files.Count+1) { throw 'ZIP file count mismatch' } } finally { $archive.Dispose() }
        if ($InnoCompiler) {
            $arguments=@('/Qp',"/DSourceDir=$stage","/DOutputDir=$WorkDir","/DMyAppVersion=$version","/DEdition=$edition","/DEditionName=$($definition.Name)","/DHasDesktop=$([int]$definition.Desktop)",(Join-Path $root 'packaging\windows\zsdesk.iss'))
            & $InnoCompiler @arguments 1>(Join-Path $WorkDir "installer.$edition.current.log")
            if ($LASTEXITCODE -ne 0) { throw "Installer compiler failed for $edition" }
        }
        # The completed payload moves only after its identity and every file hash pass.
        Assert-ChildPath $stage $WorkDir
        Assert-ChildPath $destination $OutputDir
        Move-Item -LiteralPath $stage -Destination $destination
        Move-Item -LiteralPath $zip -Destination "$destination.zip"
        if ($InnoCompiler) { Move-Item -LiteralPath (Join-Path $WorkDir "$name-setup.exe") -Destination (Join-Path $OutputDir "$name-setup.exe") }
        $state.state='verified';$state['ended']=[DateTimeOffset]::Now.ToString('o');$state['output']=$destination;$state['exe_sha256']=$files['ZSDesk.exe'].sha256
        Write-Json (Join-Path $WorkDir 'package-state.current.json') $state
        Write-Output "$name verified"
    }
} finally { $lock.Dispose() }
