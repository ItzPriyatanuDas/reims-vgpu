#Requires -Version 7.2
$ErrorActionPreference='Stop'
$PSNativeCommandUseErrorActionPreference=$false
$source=[scriptblock]::Create((Get-Content -Raw "$PSScriptRoot/../start.ps1")).Ast
$helper=$source.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Get-ReimsSshKnownHostsOption'},$true)
if(!$helper){throw 'Known-hosts formatter is missing'}
. ([scriptblock]::Create($helper.Extent.Text))
$ssh=Join-Path $env:SystemRoot 'System32/OpenSSH/ssh.exe'
$root=[IO.Path]::GetFullPath("$PSScriptRoot/../../../.work/tmp")
$paths=@(
    (Join-Path $root 'Virtual Machines/日本語 VM/state/known_hosts'),
    (Join-Path $root 'Virtual Machines/日本語 # archive/state/known_hosts'),
    (Join-Path $root (('part '*40)+'日本語/state/known_hosts'))
)
function Read-SshConfig([string]$Option) {
    $lines=@(& $ssh -G -F NUL -o BatchMode=yes -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes -o HostKeyAlias=reims-offline-fixture -o $Option fixture.invalid 2>&1 | ForEach-Object ToString)
    return @{code=$LASTEXITCODE;lines=$lines}
}
foreach($path in $paths){
    $result=Read-SshConfig (Get-ReimsSshKnownHostsOption $path)
    if($result.code -ne 0){throw "Offline ssh -G failed: $($result.lines -join ' ')"}
    if(('userknownhostsfile '+$path.Replace('\','/')) -cnotin $result.lines){throw 'OpenSSH did not preserve the entire known-hosts path'}
    if(!($result.lines -match '^stricthostkeychecking (true|yes)$') -or 'hostkeyalias reims-offline-fixture' -notin $result.lines){throw 'Strict checking or host alias changed'}
}
$comment=Read-SshConfig ('UserKnownHostsFile='+$paths[1].Replace('\','/'))
if($comment.code -ne 0 -or ('userknownhostsfile '+$paths[1].Replace('\','/')) -cin $comment.lines){throw 'Unquoted comment negative control did not reproduce truncation'}
$split=Read-SshConfig ('UserKnownHostsFile='+$paths[2].Replace('\','/'))
if($split.code -ne 255 -or !($split.lines -match 'too many userknownhostsfile entries')){throw 'Unquoted spaces negative control did not reproduce multiple paths'}
foreach($invalid in @('', "bad`"path", "bad`npath", "bad`rpath", "bad$([char]0)path")){
    $rejected=$false
    try {Get-ReimsSshKnownHostsOption $invalid | Out-Null} catch {$rejected=$true}
    if(!$rejected){throw 'Invalid path reached SSH configuration'}
}
'PASS: Windows OpenSSH -G preserves 3 quoted Unicode/space paths; strict host checking and alias retained; 2 unquoted parser failures reproduced; 5 invalid paths rejected. No connection or host-key files used.'
