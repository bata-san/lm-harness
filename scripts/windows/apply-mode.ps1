<#
.SYNOPSIS
  mode.json に書かれた希望状態 (GPU 電力制限 / vmmem 優先度) を Windows 側へ反映する。

.DESCRIPTION
  WSL 内の LLM Harness (非管理者) が FULL / SHARED モードを切り替えるときに、
  スケジュール タスク "\LLMHarness\ApplyMode" 経由で実行される。
  setup-host.ps1 が C:\ProgramData\llm-harness\ にこのファイルをコピーし、タスクを登録する。

  - 同じフォルダの mode.json を読む:
      { "mode": "full|shared",
        "gpus": [ { "index": 0, "power_limit_w": 210 | null }, ... ],
        "vmmem_priority": "Normal|BelowNormal|Idle",
        "request_id": "...", "requested_at": "..." }
  - GPU ごとに nvidia-smi -i <idx> -pl <W> (null なら power.default_limit に戻す)
  - vmmemWSL / vmmem プロセスの優先度クラスを設定
  - 結果を mode-result.json に書く: { applied_at, ok, messages[], request_id }

  セキュリティ: このスクリプトは管理者権限で動くため、mode.json から読む値は
  数値 / 列挙値だけに限定し、厳密に検証する。文字列をそのままコマンドに渡さない
  (改ざんされた JSON によるコマンド注入を防ぐ)。
#>
$ErrorActionPreference = "Continue"
$dir        = Split-Path -Parent $MyInvocation.MyCommand.Path
$modeFile   = Join-Path $dir "mode.json"
$resultFile = Join-Path $dir "mode-result.json"

$messages  = New-Object System.Collections.Generic.List[string]
$ok        = $true
$requestId = ""

function Note($m) { $script:messages.Add([string]$m) }
function Fail($m) { $script:ok = $false; $script:messages.Add("失敗: " + [string]$m) }

function Write-Result {
    try {
        $obj = [ordered]@{
            applied_at = (Get-Date).ToString("o")
            ok         = $script:ok
            messages   = @($script:messages)
            request_id = $script:requestId
        }
        $json = $obj | ConvertTo-Json -Depth 4
        $tmp = "$resultFile.tmp"
        [IO.File]::WriteAllText($tmp, $json, (New-Object Text.UTF8Encoding $false))
        Move-Item -LiteralPath $tmp -Destination $resultFile -Force
    } catch {
        # 結果が書けない場合は何もできない (Harness 側はタイムアウトとして扱う)
    }
}

function Find-NvidiaSmi {
    $cmd = Get-Command nvidia-smi.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    foreach ($p in @(
            (Join-Path $env:SystemRoot "System32\nvidia-smi.exe"),
            (Join-Path $env:ProgramFiles "NVIDIA Corporation\NVSMI\nvidia-smi.exe"))) {
        if (Test-Path -LiteralPath $p) { return $p }
    }
    return $null
}

# 整数かどうか厳密に判定 (文字列 / 小数 / bool は不可)
function Test-IntegerValue($v) {
    return ($v -is [int] -or $v -is [long])
}

# --- mode.json の読み込みと検証 ---
$cfg = $null
try {
    if (-not (Test-Path -LiteralPath $modeFile)) { throw "mode.json がありません: $modeFile" }
    $raw = [IO.File]::ReadAllText($modeFile, [Text.Encoding]::UTF8)
    $cfg = $raw | ConvertFrom-Json
} catch {
    Fail "mode.json を読めません: $($_.Exception.Message)"
    Write-Result
    exit 1
}

if ($cfg.request_id -is [string] -and $cfg.request_id -match '^[0-9A-Za-z._-]{1,64}$') {
    $requestId = $cfg.request_id
}

$modeName = [string]$cfg.mode
if ($modeName -notin @("full", "shared")) {
    Fail "mode が不正です (full / shared のみ)"
    Write-Result
    exit 1
}
Note "mode=$modeName"

# --- GPU 電力制限 ---
$nvsmi = Find-NvidiaSmi
$gpuEntries = @()
if ($null -ne $cfg.gpus) { $gpuEntries = @($cfg.gpus) }

if ($gpuEntries.Count -gt 0 -and -not $nvsmi) {
    Fail "nvidia-smi.exe が見つかりません"
} else {
    foreach ($g in $gpuEntries) {
        try {
            $idx = $g.index
            if (-not (Test-IntegerValue $idx) -or $idx -lt 0 -or $idx -gt 63) {
                Fail "GPU index が不正です"
                continue
            }
            $idx = [int]$idx
            $pl = $g.power_limit_w
            if ($null -eq $pl) {
                # 既定値へ戻す: power.default_limit を nvidia-smi から取得
                $q = & $nvsmi -i $idx --query-gpu=power.default_limit --format=csv,noheader,nounits 2>&1
                $text = ("$q").Trim()
                $d = 0.0
                if (-not [double]::TryParse($text, [Globalization.NumberStyles]::Float, [Globalization.CultureInfo]::InvariantCulture, [ref]$d)) {
                    Note "GPU ${idx}: 既定の電力制限を取得できないためスキップ ($text)"
                    continue
                }
                $pl = [int][math]::Round($d)
                $label = "既定値"
            } else {
                $label = "制限"
            }
            if (-not (Test-IntegerValue $pl) -or $pl -lt 50 -or $pl -gt 1000) {
                Fail "GPU ${idx}: 電力制限が範囲外です (50..1000 W の整数のみ)"
                continue
            }
            $pl = [int]$pl
            $out = & $nvsmi -i $idx -pl $pl 2>&1
            if ($LASTEXITCODE -eq 0) {
                Note "GPU ${idx}: 電力${label} ${pl} W に設定"
            } else {
                Fail "GPU ${idx}: nvidia-smi -pl $pl が失敗 ($(("$out").Trim()))"
            }
        } catch {
            Fail "GPU の電力制限: $($_.Exception.Message)"
        }
    }
}

# --- vmmem (WSL VM) の優先度 ---
# 許可リストの値だけを受け付ける。WMI の Priority 値 (SetPriority) も併記。
$priorityMap = @{ "Normal" = 32; "BelowNormal" = 16384; "Idle" = 64 }
$prio = [string]$cfg.vmmem_priority
if (-not $priorityMap.ContainsKey($prio)) {
    Fail "vmmem_priority が不正です (Normal / BelowNormal / Idle のみ)"
} else {
    $found = 0
    foreach ($name in @("vmmemWSL", "vmmem")) {
        try {
            $procs = @(Get-Process -Name $name -ErrorAction SilentlyContinue)
            foreach ($p in $procs) {
                $found++
                try {
                    $p.PriorityClass = [Diagnostics.ProcessPriorityClass]$prio
                    Note "${name} (PID $($p.Id)) の優先度を $prio に設定"
                } catch {
                    # 保護されたプロセスは .NET から変更できないことがある: WMI で再試行
                    try {
                        $wp = Get-CimInstance Win32_Process -Filter "ProcessId=$([int]$p.Id)"
                        $r = Invoke-CimMethod -InputObject $wp -MethodName SetPriority -Arguments @{ Priority = [uint32]$priorityMap[$prio] }
                        if ($r.ReturnValue -eq 0) {
                            Note "${name} (PID $($p.Id)) の優先度を $prio に設定 (WMI)"
                        } else {
                            Fail "${name} (PID $($p.Id)) の優先度変更に失敗 (WMI ReturnValue=$($r.ReturnValue))"
                        }
                    } catch {
                        Fail "${name} (PID $($p.Id)) の優先度変更に失敗: $($_.Exception.Message)"
                    }
                }
            }
        } catch {
            Fail "${name} の検索: $($_.Exception.Message)"
        }
    }
    if ($found -eq 0) { Note "vmmemWSL / vmmem プロセスが見つかりません (WSL 未起動?)" }
}

Write-Result
if ($ok) { exit 0 } else { exit 1 }
