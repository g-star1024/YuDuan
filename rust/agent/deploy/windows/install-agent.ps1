# 御端（YuDuan）副端 —— Windows 服务安装/卸载脚本（sc.exe 创建系统服务）。
#
# 用法（管理员 PowerShell）：
#   ./deploy/windows/install-agent.ps1               # 安装并启动
#   ./deploy/windows/install-agent.ps1 -Uninstall    # 停止并移除
#
# 前置：先构建二进制（Windows 主机或交叉编译）：
#   rustup target add x86_64-pc-windows-msvc
#   rustc -O --target x86_64-pc-windows-msvc src/main.rs -o target/agent.exe
# 注意：安装后如需修改中枢地址/验签密钥，改下面 $HubUrl / $Secret 后重跑本脚本；
#       变量写在计算机作用域（Machine），服务重启后生效。

param(
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"

$ServiceName = "yuduan-agent"
$DisplayName = "YuDuan Agent"
$BinDst     = "$env:ProgramFiles\YuDuan\yuduan-agent.exe"
$LogFile    = "$env:ProgramData\YuDuan\agent.log"

# —— 按现场环境修改 ——
$HubUrl  = "wss://hub.yuduan.internal:8443/ws/agent"
$Secret  = "CHANGE-ME-AT-DEPLOY"   # 必须与 Go 中枢 YUDUAN_HMAC_SECRET 一致

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $p  = New-Object Security.Principal.WindowsPrincipal($id)
    $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-Admin)) {
    Write-Error "请以管理员身份运行 PowerShell"
    exit 1
}

if ($Uninstall) {
    Write-Host "==> 停止并移除服务 $ServiceName"
    sc.exe stop $ServiceName | Out-Null
    Start-Sleep -Seconds 2
    sc.exe delete $ServiceName | Out-Null
    Remove-Item $BinDst -Force -ErrorAction SilentlyContinue
    [Environment]::SetEnvironmentVariable("YUDUAN_HUB", $null, "Machine")
    [Environment]::SetEnvironmentVariable("YUDUAN_HMAC_SECRET", $null, "Machine")
    Write-Host "==> 已卸载（日志保留：$LogFile）"
    exit 0
}

$src = Join-Path $PSScriptRoot "..\target\agent.exe"
if (-not (Test-Path $src)) {
    Write-Error "未找到 $src，请先构建：rustc -O --target x86_64-pc-windows-msvc src/main.rs -o target/agent.exe"
    exit 1
}

Write-Host "==> 安装二进制 -> $BinDst"
New-Item -ItemType Directory -Force -Path (Split-Path $BinDst) | Out-Null
Copy-Item $src $BinDst -Force

Write-Host "==> 写入环境变量（Machine 作用域，服务重启后生效）"
[Environment]::SetEnvironmentVariable("YUDUAN_HUB", $HubUrl, "Machine")
[Environment]::SetEnvironmentVariable("YUDUAN_HMAC_SECRET", $Secret, "Machine")

Write-Host "==> 创建服务（sc.exe，开机自启，失败自动重启）"
# 注意：sc.exe 的 = 后空格是语法要求
$create = sc.exe create $ServiceName binPath= "`"$BinDst`"" start= auto DisplayName= "`"$DisplayName`" obj= "LocalSystem"
if ($LASTEXITCODE -ne 0) {
    # 已存在则先删后建
    sc.exe delete $ServiceName | Out-Null
    Start-Sleep -Seconds 2
    sc.exe create $ServiceName binPath= "`"$BinDst`"" start= auto DisplayName= "`"$DisplayName`"" obj= "LocalSystem" | Out-Null
}
# 崩溃恢复策略：失败后 5s 重启
sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/5000/restart/5000 | Out-Null

Write-Host "==> 启动服务"
sc.exe start $ServiceName | Out-Null
Start-Sleep -Seconds 2
sc.exe query $ServiceName | Out-Null
Write-Host "==> 完成。状态：Get-Service $ServiceName；日志：$LogFile（副端 stderr 重定向见 README）"
