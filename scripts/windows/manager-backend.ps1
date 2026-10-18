#Requires -Version 7.2
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Root,
    [string]$VmDirectory,
    [ValidateSet('Status','Setup','ImportDisk','GetIdentity','SetIdentity','PrepareUpdates','ListUsb','InstallUsbDk','AttachUsb','DetachUsb','BuildEngine','EnableWhpx','Start','PowerOff','Restart','Exit','ForceExit','Pause','Resume','SnapshotCreate','SnapshotRestore','GenerateIdentity','EnableAppleAccount','DisableAppleAccount','SetResolution','ApplyResolution','SetNetwork','SetHardware','ExpandDisk','OptimizeDisk','CreateBridge','InstallTap','Pair','CollectLogs')][string]$Action = 'Status',
    [string]$DiskPath,
    [ValidateRange(0,255)][int]$UsbBus, [ValidateRange(1,127)][int]$UsbAddress = 1,
    [string]$UsbId,
    [string]$Iso,
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$')][string]$Name,
    [ValidateSet('1280x720','1920x1080','2560x1440','3840x2160')][string]$Resolution = '1920x1080',
    [ValidateSet('nat','bridge')][string]$Network = 'nat',
    [string]$Adapter, [string]$PhysicalAdapter, [string]$Installer,
    [ValidatePattern('^[a-z_][a-z0-9_-]*$')][string]$User,
    [string]$GuestHost, [string]$Fingerprint,
    [ValidateRange(1,64)][int]$Cpus = 8,
    [ValidateRange(4096,131072)][int]$MemoryMiB = 8192,
    [ValidateRange(64,2048)][int]$DiskGiB = 128,
    [ValidateSet('none','dsound')][string]$Audio = 'none',
    [switch]$Microphone,
    [ValidateSet('winusb','usbdk')][string]$UsbBackend = 'winusb',
    [switch]$WithInstaller
)
$env:REIMS_PROJECT_ROOT_OVERRIDE = [IO.Path]::GetFullPath($Root)
$env:REIMS_VM_ROOT_OVERRIDE = if ($VmDirectory) { [IO.Path]::GetFullPath($VmDirectory) } else { '' }
. "$PSScriptRoot/start.ps1" -FunctionsOnly

function Get-GuestEndpoint($Config) {
    if ((Get-ReimsNetworkMode $Config) -eq 'nat') { return @{host='127.0.0.1';port=$Config.ssh_port} }
    if ($Config.network.guest_host -notmatch '^[A-Za-z0-9][A-Za-z0-9.:-]*$') { throw 'ブリッジの macOS IP アドレスを設定してください。' }
    return @{host=$Config.network.guest_host;port=22}
}

function Invoke-GuestControl([string]$Operation, [string]$Command) {
    $runtime = Get-ReimsRuntime
    if (!$runtime -or !(Get-ReimsVmProcess $runtime)) { throw 'VM は停止しています。' }
    $state = (Invoke-ReimsQmp $runtime 'query-status').status
    if ($state -eq 'shutdown' -and $Operation -eq '-h') { 'macOS は電源オフ済みです。'; return }
    if ($state -ne 'running') { throw "VM は $state です。電源操作の前に再開してください。" }
    $guestPath = Join-Path $VmRoot 'state/guest.json'
    $setupPath = Join-Path $VmRoot 'state/setup.json'
    $account = if (Test-Path $guestPath) { (Get-Content $guestPath -Raw | ConvertFrom-Json).user }
        elseif (Test-Path $setupPath) { (Get-Content $setupPath -Raw | ConvertFrom-Json).account_name }
        else { throw 'macOS で ReimsTools の setup.command を実行し、接続タブでペアリングしてください。' }
    if ($account -notmatch '^[a-z_][a-z0-9_-]*$') { throw 'Invalid guest account name.' }
    $key = Join-Path $VmRoot 'state/ssh/id_ed25519'
    $known = Join-Path $VmRoot 'state/known_hosts'
    if (!(Test-Path $key) -or !(Test-Path $known)) { throw 'SSH キーとホスト鍵が未設定です。' }
    $endpoint = Get-GuestEndpoint (Get-ReimsVmConfig)
    $guest = if (Test-Path $guestPath) { Get-Content $guestPath -Raw | ConvertFrom-Json } else { $null }
    $alias = if ($guest -and $guest.PSObject.Properties['host_key_alias']) { @('-o', "HostKeyAlias=$($guest.host_key_alias)") } else { @() }
    $output = @(& "$env:SystemRoot/System32/OpenSSH/ssh.exe" -F NUL -n -o ConnectTimeout=5 -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes `
        -o (Get-ReimsSshKnownHostsOption $known) -o KexAlgorithms=curve25519-sha256 @alias -p $endpoint.port -i $key `
        "$account@$($endpoint.host)" $(if ($Command) {$Command} else {"sudo -n /sbin/shutdown $Operation now"}) 2>&1 | ForEach-Object ToString)
    $exitCode = $LASTEXITCODE
    if ($Command) {
        if ($exitCode -ne 0) { throw "macOS の操作に失敗しました ($exitCode): $($output -join ' ')" }
        return $output -join "`n"
    }
    if ($Operation -eq '-h') {
        $deadline = [DateTime]::UtcNow.AddSeconds(180)
        while ([DateTime]::UtcNow -lt $deadline) {
            if (!(Get-ReimsVmProcess $runtime) -or (Invoke-ReimsQmp $runtime 'query-status').status -eq 'shutdown') { 'macOS の電源オフを確認しました。'; return }
            Start-Sleep -Milliseconds 250
        }
    } elseif ($exitCode -eq 0 -or ($output -join "`n") -match '(?m)^Shutdown NOW!') {
        'macOS が再起動コマンドを受け付けました。'; return
    }
    throw "SSH 電源操作を確認できませんでした ($exitCode): $($output -join ' ')"
}

function Assert-Administrator {
    $principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
    if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'この操作には Windows の管理者権限が必要です。' }
}

function Get-NewVmPorts {
    $reserved = [Collections.Generic.HashSet[int]]::new()
    $legacy=Join-Path $ProjectRoot 'vm/state/config.json'
    if (Test-Path $legacy) {
        $legacyConfig=Get-Content -Raw $legacy | ConvertFrom-Json
        [void]$reserved.Add([int]$legacyConfig.qmp_port); [void]$reserved.Add([int]$legacyConfig.ssh_port)
    }
    $library = Join-Path $ProjectRoot '.work/manager-library.json'
    if (Test-Path $library) {
        foreach ($file in (Get-Content -Raw $library | ConvertFrom-Json)) {
            if (!(Test-Path -LiteralPath $file)) { continue }
            $entry = Get-Content -Raw -LiteralPath $file | ConvertFrom-Json
            $directory = [IO.Path]::GetFullPath($entry.directory, (Split-Path $file))
            $configFile = Join-Path $directory 'state/config.json'
            if (Test-Path $configFile) {
                $entryConfig = Get-Content -Raw $configFile | ConvertFrom-Json
                [void]$reserved.Add([int]$entryConfig.qmp_port); [void]$reserved.Add([int]$entryConfig.ssh_port)
            }
        }
    }
    $result = @()
    for ($port = 4444; $port -lt 65000 -and $result.Count -lt 2; $port++) {
        if ($reserved.Contains($port)) { continue }
        $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,$port)
        try { $listener.Start(); $result += $port } catch [Net.Sockets.SocketException] { }
        finally { $listener.Stop() }
    }
    if ($result.Count -ne 2) { throw 'VM の接続ポートを確保できません。' }
    return $result
}

if ($Action -eq 'Status') {
    $runtime = Get-ReimsRuntime
    $prepared = Test-Path (Join-Path $VmRoot 'state/config.json')
    $state = if ($prepared) { 'stopped' } else { 'unprepared' }
    if ($runtime -and (Get-ReimsVmProcess $runtime)) { $state = (Invoke-ReimsQmp $runtime 'query-status').status }
    $snapshots = @(& "$PSScriptRoot/snapshot.ps1" -Action List | ForEach-Object name)
    $config = if ($prepared) { Get-ReimsVmConfig } else { $null }
    $adapters = @(Get-NetAdapter | Select-Object Name,InterfaceDescription,Status,HardwareInterface)
    $taps = @(Get-ReimsTapAdapters | ForEach-Object Name)
    $installerAttached = $false
    if ($config -and $config.PSObject.Properties['connect_installer']) { $installerAttached = [bool]$config.connect_installer }
    if ($runtime -and (Get-ReimsVmProcess $runtime)) {
        $launch = Get-Content -Raw (Join-Path $runtime.run_directory 'command.json') | ConvertFrom-Json
        $installerAttached = [bool]($launch.arguments | Where-Object { $_ -like 'id=installer,*' })
    }
    @{state=$state; prepared=$prepared; snapshots=$snapshots; taps=$taps; adapters=$adapters;
        runtime=$(if ($runtime -and (Get-ReimsVmProcess $runtime)) {$runtime} else {$null});
        vm_directory=$VmRoot;
        microphone=[bool]($config -and $config.PSObject.Properties['microphone'] -and $config.microphone);
        usb_backend=$(if ($config -and $config.PSObject.Properties['usb_backend']) {$config.usb_backend} else {'winusb'});
        cpus=$(if ($config) {$config.cpus} else {8}); memory_mib=$(if ($config) {$config.memory_mib} else {8192});
        disk_gib=$(if ($config) {$config.disk_gib} else {128}); disk=$(if ($config) {$config.active_disk} else {''});
        audio=$(if ($config -and $config.PSObject.Properties['audio']) {$config.audio} else {'none'});
        with_installer=$installerAttached; network_adapter=$(if ($config -and $config.PSObject.Properties['network']) {$config.network.adapter} else {''});
        guest_host=$(if ($config -and $config.PSObject.Properties['network']) {$config.network.guest_host} else {''});
        resolution=$(if ($config) {$config.resolution} else {'1920x1080'});
        network=$(if ($config) {Get-ReimsNetworkMode $config} else {'nat'});
        iso=$(if ($config) {$config.iso} else {''})} | ConvertTo-Json -Depth 5 -Compress
    return
}
switch ($Action) {
    ImportDisk {
        Assert-ReimsVmStopped
        if (Test-Path (Join-Path $VmRoot 'state/config.json')) { throw '既存の VM に上書きせず、新しい VM を作成してください。' }
        $source = Get-Item -LiteralPath $DiskPath
        if ($source.PSIsContainer) { throw '仮想ディスクのファイルを選択してください。' }
        $tool = Get-ReimsTool 'qemu-img.exe'
        $info = (Invoke-ReimsTool $tool @('info','--output=json',$source.FullName)) -join "`n" | ConvertFrom-Json
        if ($info.format -notin @('qcow2','vmdk','raw') -or $info.'virtual-size' -le 0) { throw 'QCOW2、VMDK、RAW の仮想ディスクを選択してください。' }
        $disks = Join-Path $VmRoot 'disks'
        New-Item -ItemType Directory -Force -Path $disks | Out-Null
        $base = Join-Path $disks 'base.qcow2'; $active = Join-Path $disks 'current.qcow2'
        if ((Test-Path $base) -or (Test-Path "$base.partial") -or (Test-Path $active)) { throw '取り込み先には既にディスクがあります。新しい保存先を選択してください。' }
        $locked = [IO.File]::Open($source.FullName,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
        try { Invoke-ReimsTool $tool @('convert','-f',$info.format,'-O','qcow2',$source.FullName,"$base.partial") | Out-Null }
        finally { $locked.Dispose() }
        Invoke-ReimsTool $tool @('check','-q',"$base.partial") | Out-Null
        Move-Item -LiteralPath "$base.partial" -Destination $base
        Invoke-ReimsTool $tool @('create','-f','qcow2','-F','qcow2','-b',$base,$active) | Out-Null
        & "$PSScriptRoot/prepare-opencore.ps1" | Out-Null
        $oc = Get-Content -Raw (Join-Path $VmRoot 'state/opencore.json') | ConvertFrom-Json
        $vars = Join-Path $VmRoot 'firmware/OVMF_VARS.fd'
        Copy-Item -LiteralPath $oc.firmware_vars_template -Destination $vars
        $ports = Get-NewVmPorts
        $random = [Security.Cryptography.RandomNumberGenerator]::GetBytes(3)
        $config = @{
            schema=1;cpus=4;memory_mib=8192;resolution='1920x1080';disk_gib=[math]::Ceiling($info.'virtual-size'/1GB)
            cpu='Haswell-noTSX,stepping=3,vendor=GenuineIntel,+ssse3,+sse4.2,+popcnt,+avx,+avx2,+aes,+xsave,+xsaveopt,-arat,enforce'
            mac='52:54:00:'+(($random | ForEach-Object {$_.ToString('x2')}) -join ':');qmp_port=$ports[0];ssh_port=$ports[1]
            iso='';media_kind='cdrom';connect_installer=$false;base_disk=$base;active_disk=$active
            opencore=$oc.image;firmware_code=$oc.firmware_code;firmware_vars=$vars;audio='dsound';microphone=$false
            rom=Join-Path $ProjectRoot '.work/target/dist-vulkan/share/reims-vgpu/ReimsVgpu.rom'
        }
        Write-ReimsJson (Join-Path $VmRoot 'state/config.json') $config
        & "$PSScriptRoot/prepare-opencore.ps1" -GenerateIdentity -Rebuild | Out-Null
        & "$PSScriptRoot/prepare-tools.ps1" | Out-Null
        'ディスクのコピーを新しい VM に取り込みました。元のファイルは変更していません。'
    }
    InstallUsbDk { & "$PSScriptRoot/install-usbdk.ps1" }
    ListUsb { ConvertTo-Json -InputObject @(Get-ReimsUsbDevices) -Depth 4 -Compress }
    AttachUsb {
        if (!$UsbId) { $UsbId=(Get-ReimsUsbDevices | Where-Object { $_.bus -eq $UsbBus -and $_.address -eq $UsbAddress } | Select-Object -First 1).id }
        Set-ReimsUsbSelection $UsbId $true
    }
    DetachUsb { Set-ReimsUsbSelection $UsbId $false }
    PrepareUpdates { & "$PSScriptRoot/prepare-update-compatibility.ps1" }
    GetIdentity {
        $manifestPath = Join-Path $VmRoot 'state/opencore.json'
        if (!(Test-Path $manifestPath)) { throw '先に VM のセットアップを完了してください。' }
        $manifest = Get-Content -Raw $manifestPath | ConvertFrom-Json
        [void](Assert-ReimsArtifactPath $manifest.config)
        $reader = @'
import json,plistlib,sys
g=plistlib.load(open(sys.argv[1],'rb'))['PlatformInfo']['Generic']
print(json.dumps(dict(model=g['SystemProductName'],serial=g['SystemSerialNumber'],mlb=g['MLB'],uuid=g['SystemUUID'],rom=g['ROM'].hex())))
'@
        $reader | & (Get-ReimsTool 'python.exe') - $manifest.config
        if ($LASTEXITCODE -ne 0) { throw '識別情報を読み込めません。' }
    }
    SetIdentity {
        Assert-ReimsVmStopped
        $temporary = Join-Path $ProjectRoot ('.work/tmp/identity-' + [guid]::NewGuid() + '.json')
        try {
            $json = [Console]::In.ReadToEnd()
            if ($json.Length -gt 4096) { throw 'Identity data is too large.' }
            [IO.File]::WriteAllText($temporary, $json, [Text.UTF8Encoding]::new($false))
            & "$PSScriptRoot/prepare-opencore.ps1" -IdentityFile $temporary -Rebuild | Out-Null
            '識別情報を保存しました。次回起動時に適用します。'
        } finally { if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary } }
    }
    BuildEngine {
        Assert-ReimsVmStopped
        if (!(Test-Path (Join-Path $ProjectRoot 'Cargo.toml')) -or (Test-Path (Join-Path $ProjectRoot '.git') -PathType Leaf)) { throw '本体リポジトリの通常のチェックアウトを選択してください。' }
        & "$PSScriptRoot/bootstrap.ps1"
        & "$PSScriptRoot/build.ps1"
    }
    EnableWhpx {
        Assert-ReimsVmStopped
        Assert-Administrator
        & "$PSScriptRoot/enable-whpx.ps1"
    }
    Setup {
        Assert-ReimsVmStopped
        if (!$Iso) { throw 'macOS インストーラーを選択してください。' }
        & "$PSScriptRoot/prepare-opencore.ps1" -Rebuild | Out-Null
        $ports = Get-NewVmPorts
        & "$PSScriptRoot/prepare-vm.ps1" -Iso $Iso -DiskGiB $DiskGiB -QmpPort $ports[0] -SshPort $ports[1] | Out-Null
        if (!(Test-Path (Join-Path $VmRoot 'state/opencore-identity.json'))) {
            & "$PSScriptRoot/prepare-opencore.ps1" -GenerateIdentity -Rebuild | Out-Null
        }
        & "$PSScriptRoot/prepare-tools.ps1"
        '準備完了。インストーラー付きで起動し、macOS インストール後に ReimsTools の setup.command を実行してください。'
    }
    Start { & "$PSScriptRoot/start.ps1" -WithoutInstaller:(!$WithInstaller) }
    PowerOff { Invoke-GuestControl '-h'; & "$PSScriptRoot/stop.ps1" }
    Restart { Invoke-GuestControl '-r' }
    Exit {
        $runtime = Get-ReimsRuntime
        if ($runtime -and (Get-ReimsVmProcess $runtime) -and (Invoke-ReimsQmp $runtime 'query-status').status -ne 'shutdown') { Invoke-GuestControl '-h' }
        & "$PSScriptRoot/stop.ps1" -Exit
    }
    ForceExit { & "$PSScriptRoot/stop.ps1" -Force }
    Pause { & "$PSScriptRoot/qmp.ps1" -Command stop }
    Resume {
        $runtime=Get-ReimsRuntime
        $command=if ((Invoke-ReimsQmp $runtime query-status).status -eq 'suspended') {'system_wakeup'} else {'cont'}
        Invoke-ReimsQmp $runtime $command | Out-Null
    }
    SnapshotCreate { & "$PSScriptRoot/snapshot.ps1" -Action Create -Name $Name }
    SnapshotRestore { & "$PSScriptRoot/snapshot.ps1" -Action Restore -Name $Name }
    GenerateIdentity { & "$PSScriptRoot/prepare-opencore.ps1" -GenerateIdentity -Rebuild }
    EnableAppleAccount { & "$PSScriptRoot/prepare-opencore.ps1" -AppleAccountCompatibility Enable -Rebuild }
    DisableAppleAccount { & "$PSScriptRoot/prepare-opencore.ps1" -AppleAccountCompatibility Disable -Rebuild }
    SetResolution {
        Assert-ReimsVmStopped
        $config = Get-ReimsVmConfig
        $config.resolution = $Resolution
        Write-ReimsJson (Join-Path $VmRoot 'state/config.json') $config
        '解像度を保存しました。次回起動時に適用します。'
    }
    ApplyResolution {
        $js = Get-Content -Raw -LiteralPath "$PSScriptRoot/guest-display.js"
        Invoke-GuestControl -Command "/usr/bin/osascript -l JavaScript - $Resolution <<'REIMS_DISPLAY'`n$js`nREIMS_DISPLAY"
    }
    SetNetwork {
        Assert-ReimsVmStopped
        if ($Network -eq 'bridge' -and $Adapter -notin @(Get-ReimsTapAdapters | ForEach-Object Name)) { throw '実在する TAP-Windows アダプターを選択してください。' }
        if ($GuestHost -and $GuestHost -notmatch '^[A-Za-z0-9][A-Za-z0-9.:-]*$') { throw 'Invalid macOS IP / hostname.' }
        $config = Get-ReimsVmConfig
        $config | Add-Member -Force network @{mode=$Network; adapter=$Adapter; guest_host=$GuestHost}
        Write-ReimsJson (Join-Path $VmRoot 'state/config.json') $config
        'ネットワーク設定を保存しました。次回起動時に適用します。'
    }
    SetHardware {
        Assert-ReimsVmStopped
        if ($MemoryMiB % 4 -ne 0) { throw 'メモリは 4 MiB 単位で指定してください。' }
        $hostInfo = Get-CimInstance Win32_ComputerSystem
        if ($Cpus -gt $hostInfo.NumberOfLogicalProcessors -or [long]$MemoryMiB * 1MB -ge $hostInfo.TotalPhysicalMemory) { throw 'ホストの CPU / メモリ容量以内で設定してください。' }
        if ($Network -eq 'bridge' -and $Adapter -notin @(Get-ReimsTapAdapters | ForEach-Object Name)) { throw 'TAP adapter not found.' }
        if ($GuestHost -and $GuestHost -notmatch '^[A-Za-z0-9][A-Za-z0-9.:-]*$') { throw 'Invalid guest address.' }
        if ($Iso) { & "$PSScriptRoot/prepare-vm.ps1" -Iso $Iso | Out-Null }
        $config = Get-ReimsVmConfig
        $config.cpus = $Cpus; $config.memory_mib = $MemoryMiB; $config.resolution = $Resolution
        $config | Add-Member -Force audio $Audio
        $config | Add-Member -Force microphone ([bool]$Microphone)
        $config | Add-Member -Force usb_backend $UsbBackend
        $config | Add-Member -Force connect_installer ([bool]$WithInstaller)
        $config | Add-Member -Force network @{mode=$Network;adapter=$Adapter;guest_host=$GuestHost}
        Write-ReimsJson (Join-Path $VmRoot 'state/config.json') $config
        '仮想マシン設定を保存しました。次回起動に適用します。'
    }
    ExpandDisk {
        Assert-ReimsVmStopped
        $config = Get-ReimsVmConfig
        $tool = Get-ReimsTool 'qemu-img.exe'
        $info = (Invoke-ReimsTool $tool @('info','--output=json',$config.active_disk)) -join "`n" | ConvertFrom-Json
        if ([long]$DiskGiB * 1GB -le $info.'virtual-size') { throw '現在より大きな容量を指定してください。縮小は行いません。' }
        $next = Join-Path $VmRoot ('disks/' + [guid]::NewGuid() + '.qcow2')
        Invoke-ReimsTool $tool @('check','-q',$config.active_disk) | Out-Null
        Invoke-ReimsTool $tool @('create','-f','qcow2','-F','qcow2','-b',$config.active_disk,$next) | Out-Null
        Invoke-ReimsTool $tool @('resize',$next,"$($DiskGiB)G") | Out-Null
        $config.active_disk = $next; $config.disk_gib = $DiskGiB
        Write-ReimsJson (Join-Path $VmRoot 'state/config.json') $config
        '仮想ディスクを拡張しました。macOS の Disk Utility でも APFS 領域を拡張してください。以前のディスクは保持しています。'
    }
    OptimizeDisk {
        Assert-ReimsVmStopped
        $config = Get-ReimsVmConfig
        $tool = Get-ReimsTool 'qemu-img.exe'
        $next = Join-Path $VmRoot ('disks/' + [guid]::NewGuid() + '.qcow2')
        Invoke-ReimsTool $tool @('convert','-f','qcow2','-O','qcow2',$config.active_disk,"$next.partial") | Out-Null
        Invoke-ReimsTool $tool @('rebase','-f','qcow2','-b',$config.base_disk,'-F','qcow2',"$next.partial") | Out-Null
        Invoke-ReimsTool $tool @('check','-q',"$next.partial") | Out-Null
        Invoke-ReimsTool $tool @('compare','-f','qcow2','-F','qcow2',$config.active_disk,"$next.partial") | Out-Null
        Move-Item -LiteralPath "$next.partial" -Destination $next
        $config.active_disk = $next
        Write-ReimsJson (Join-Path $VmRoot 'state/config.json') $config
        'ディスク差分をまとめ、内容の一致を検証しました。以前のディスクとスナップショットは保持されています。'
    }
    CreateBridge {
        Assert-ReimsVmStopped
        Assert-Administrator
        if ($Adapter -notin @(Get-ReimsTapAdapters | ForEach-Object Name)) { throw 'TAP adapter not found.' }
        $physical = @(Get-NetAdapter | Where-Object { $_.Name -eq $PhysicalAdapter -and $_.HardwareInterface })
        if ($physical.Count -ne 1) { throw '実在する物理ネットワークアダプターを選択してください。' }
        Invoke-ReimsTool "$env:SystemRoot/System32/netsh.exe" @('bridge','create',$Adapter,$PhysicalAdapter)
    }
    InstallTap {
        Assert-Administrator
        $file = Get-Item -LiteralPath $Installer
        if ($file.Extension -ne '.exe') { throw '公式 TAP-Windows インストーラー (.exe) を選択してください。' }
        $signature = Get-AuthenticodeSignature -LiteralPath $file.FullName
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'OpenVPN') { throw '有効な OpenVPN 署名のないインストーラーは実行しません。' }
        $process = Start-Process -FilePath $file.FullName -ArgumentList '/S' -WindowStyle Hidden -PassThru -Wait
        if ($process.ExitCode -ne 0) { throw "TAP installer failed ($($process.ExitCode))." }
    }
    Pair {
        if (!$User -or $Fingerprint -notmatch '^SHA256:[A-Za-z0-9+/]{43}$') { throw 'macOS のユーザー名と setup.command が表示した SHA256 指紋を入力してください。' }
        $endpoint = Get-GuestEndpoint (Get-ReimsVmConfig)
        $scanner = Join-Path $MsysRoot 'usr/bin/ssh-keyscan.exe'
        $scan = @(& $scanner -T 5 -t ed25519 -p $endpoint.port $endpoint.host 2>$null | Where-Object { $_ -match '^[^#].* ssh-ed25519 ' })
        if ($LASTEXITCODE -ne 0 -or $scan.Count -ne 1) { throw 'macOS SSH ホスト鍵を取得できませんでした。' }
        $blob = [Convert]::FromBase64String(($scan[0] -split '\s+')[2])
        $actual = 'SHA256:' + [Convert]::ToBase64String([Security.Cryptography.SHA256]::HashData($blob)).TrimEnd('=')
        if ($actual -cne $Fingerprint) { throw 'ホスト鍵の指紋が一致しません。保存しませんでした。' }
        $known = Join-Path $VmRoot 'state/known_hosts'
        $alias = 'reims-macos'
        $line = $alias + ' ssh-ed25519 ' + [Convert]::ToBase64String($blob)
        $lines = if (Test-Path $known) { @(Get-Content $known | Where-Object { $_ -notlike "$alias *" }) } else { @() }
        @($lines) + $line | Set-Content -LiteralPath $known -Encoding utf8NoBOM
        Write-ReimsJson (Join-Path $VmRoot 'state/guest.json') @{user=$User;host_key_alias=$alias}
        'SSH ホスト鍵を照合し、接続設定を保存しました。'
    }
    CollectLogs { & "$PSScriptRoot/collect-logs.ps1" -Screenshot }
}
