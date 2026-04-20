# ColGREP Windows GPU Integration Test Guide

**Target machine:** Windows, NVIDIA A2000 (8GB VRAM), 32GB RAM
**Purpose:** Validate colgrep build, indexing, search, and daemon with GPU acceleration
**Repo:** `https://github.com/ASCE-D/next-plaid.git`
**Branch:** `feat/colgrep-daemon-with-local-changes`

---

## Prerequisites

Install the following before starting:

- **Git for Windows:** https://git-scm.com/download/win
- **Rust toolchain:** https://rustup.rs (download rustup-init.exe, use default MSVC toolchain)
- **Visual Studio Build Tools:** https://visualstudio.microsoft.com/visual-studio-cpp-build-tools/ (select "Desktop development with C++")
- **Python 3:** https://python.org (for json formatting in tests, add to PATH)
- **curl:** Comes with Windows 10/11 by default

### Option A: DirectML (Recommended — Zero GPU Setup)

No additional GPU software needed. DirectML ships with Windows 10/11. This is the easiest path.

### Option B: CUDA (10-15% faster, more setup)

Only if you want CUDA specifically:
- **CUDA Toolkit 12.x:** https://developer.nvidia.com/cuda-downloads
- **cuDNN 9.x:** https://developer.nvidia.com/cudnn (requires NVIDIA account, copy DLLs to CUDA bin dir)

---

## Phase 0: Environment Verification

Open **PowerShell** (or Windows Terminal) and run:

```powershell
# 1. Check NVIDIA driver
nvidia-smi
# Expected: Shows A2000, driver version, CUDA version
# Record: GPU model, driver version, CUDA version, VRAM

# 2. Check Rust
rustc --version
cargo --version
# Expected: rustc 1.xx, cargo 1.xx

# 3. Check Visual Studio build tools
cl
# Expected: Shows Microsoft C/C++ compiler version
# If "not recognized": open "x64 Native Tools Command Prompt for VS" instead

# 4. Check GPU memory
nvidia-smi --query-gpu=name,memory.total,memory.free --format=csv,noheader
# Expected: NVIDIA RTX A2000, 8192 MiB, ~7xxx MiB free

# 5. (CUDA only) Check CUDA toolkit
nvcc --version
# Expected: CUDA compilation tools version

# 6. (CUDA only) Check cuDNN
where cudnn64_*.dll 2>$null
# Or check: dir "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.*\bin\cudnn*.dll"
```

**STOP if step 1 or 2 fails.** Fix driver/Rust installation before proceeding.

---

## Phase 1: Clone and Build

```powershell
# 1. Clone the repo
git clone https://github.com/ASCE-D/next-plaid.git
cd next-plaid
git checkout feat/colgrep-daemon-with-local-changes

# 2. Verify branch
git log --oneline -5
# Expected: Latest commit "docs: add CUDA GPU test guide and updated plans" or newer

# 3a. Build with DirectML (recommended)
$env:CMAKE_GENERATOR = "Visual Studio 17 2022"
cargo build --release -p colgrep --features "directml" 2>&1 | Tee-Object -FilePath C:\temp\build-directml.log
# Expected: "Finished release profile" with no errors

# 3b. OR build with CUDA (if CUDA toolkit + cuDNN installed)
# cargo build --release -p colgrep --features "cuda" 2>&1 | Tee-Object -FilePath C:\temp\build-cuda.log

# 4. Copy binary
New-Item -ItemType Directory -Force -Path C:\temp | Out-Null
Copy-Item target\release\colgrep.exe C:\temp\colgrep-gpu.exe

# 5. Verify
C:\temp\colgrep-gpu.exe --version
# Expected: colgrep 1.2.0

C:\temp\colgrep-gpu.exe serve --help
# Expected: Shows serve subcommand help

# 6. Record binary size
(Get-Item C:\temp\colgrep-gpu.exe).Length / 1MB
# Expected: ~50-70 MB
```

### Build Troubleshooting

**"link.exe not found"** — Run from "x64 Native Tools Command Prompt for VS" or:
```powershell
& "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
```

**CUDA linker errors** — Ensure CUDA bin is in PATH:
```powershell
$env:PATH += ";C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6\bin"
```

**cmake errors** — Install CMake: `winget install Kitware.CMake`

---

## Phase 2: GPU Runtime Verification

```powershell
# 1. Create test project
New-Item -ItemType Directory -Force -Path C:\temp\gpu-test-project\src | Out-Null
for ($i = 1; $i -le 20; $i++) {
    @"
/// Memory allocation helper module $i
pub fn allocate_$i(size: usize) -> Vec<u8> {
    vec![0u8; size]
}
"@ | Out-File -Encoding utf8 "C:\temp\gpu-test-project\src\mod_$i.rs"
}

# 2. Index with GPU (DirectML auto-detects, or use --force-gpu for CUDA)
C:\temp\colgrep-gpu.exe init -y C:\temp\gpu-test-project 2>&1 | Tee-Object -FilePath C:\temp\gpu-verify.log
# Expected: Indexes successfully
# Check log for "Runtime:" line — should NOT say "CPU" if GPU is working

# 3. (CUDA only) Force GPU to verify CUDA is used
# C:\temp\colgrep-gpu.exe --force-gpu init -y C:\temp\gpu-test-project
# Expected: Succeeds without "CUDA support not compiled" error

# 4. Search test
C:\temp\colgrep-gpu.exe "memory allocation" C:\temp\gpu-test-project -k 3
# Expected: Returns results from test files

# 5. Clean up
C:\temp\colgrep-gpu.exe clear C:\temp\gpu-test-project
```

---

## Phase 3: GPU vs CPU Benchmark

Use the repo itself as test data.

```powershell
$PROJECT = (Get-Location).Path

# --- CPU Baseline ---
Write-Host "=== CPU Indexing ==="
C:\temp\colgrep-gpu.exe clear $PROJECT 2>$null
$cpuStart = Get-Date
$env:COLGREP_FORCE_CPU = "1"
C:\temp\colgrep-gpu.exe init -y $PROJECT 2>&1 | Tee-Object -FilePath C:\temp\index-cpu.log
Remove-Item Env:\COLGREP_FORCE_CPU
$cpuTime = (Get-Date) - $cpuStart
Write-Host "CPU time: $($cpuTime.TotalSeconds) seconds"

C:\temp\colgrep-gpu.exe "memory allocation" $PROJECT -k 3

# --- GPU ---
Write-Host "=== GPU Indexing ==="
C:\temp\colgrep-gpu.exe clear $PROJECT
$gpuStart = Get-Date
C:\temp\colgrep-gpu.exe init -y $PROJECT 2>&1 | Tee-Object -FilePath C:\temp\index-gpu.log
$gpuTime = (Get-Date) - $gpuStart
Write-Host "GPU time: $($gpuTime.TotalSeconds) seconds"

C:\temp\colgrep-gpu.exe "memory allocation" $PROJECT -k 3

# --- Compare ---
Write-Host ""
Write-Host "=== Results ==="
Write-Host "CPU: $($cpuTime.TotalSeconds)s"
Write-Host "GPU: $($gpuTime.TotalSeconds)s"
Write-Host "Speedup: $([math]::Round($cpuTime.TotalSeconds / $gpuTime.TotalSeconds, 2))x"
```

---

## Phase 4: Chunked Indexing (Memory Bounded)

```powershell
$PROJECT = (Get-Location).Path

C:\temp\colgrep-gpu.exe clear $PROJECT
$start = Get-Date
C:\temp\colgrep-gpu.exe init -y --chunked --chunk-files 100 $PROJECT 2>&1 | Tee-Object -FilePath C:\temp\index-chunked.log
$elapsed = (Get-Date) - $start
Write-Host "Chunked time: $($elapsed.TotalSeconds) seconds"
# Expected: Shows "Wave N:" progress messages

# Verify search
C:\temp\colgrep-gpu.exe "thread safety" $PROJECT -k 5
```

---

## Phase 5: Daemon Testing

```powershell
$PROJECT = (Get-Location).Path

# Ensure index exists
C:\temp\colgrep-gpu.exe status $PROJECT

# Start daemon in background
$daemon = Start-Process -FilePath "C:\temp\colgrep-gpu.exe" `
    -ArgumentList "serve","--port","9090","--index",$PROJECT,"--sessions","4" `
    -PassThru -RedirectStandardOutput "C:\temp\serve.log" -RedirectStandardError "C:\temp\serve-err.log"
Write-Host "Daemon PID: $($daemon.Id)"
Start-Sleep -Seconds 15

# --- Health ---
Write-Host "=== Health ==="
curl -s http://localhost:9090/health | python -m json.tool

# --- Basic search ---
Write-Host "=== Basic Search ==="
curl -s -X POST http://localhost:9090/search `
  -H "Content-Type: application/json" `
  -d '{\"query\": \"memory allocation\", \"top_k\": 5}' | python -m json.tool

# --- Target paths ---
Write-Host "=== Target Paths ==="
curl -s -X POST http://localhost:9090/search `
  -H "Content-Type: application/json" `
  -d '{\"query\": \"embedding model\", \"top_k\": 5, \"target_paths\": [\"next-plaid-onnx/src\"]}' | python -m json.tool

# --- Include patterns ---
Write-Host "=== Include Patterns ==="
curl -s -X POST http://localhost:9090/search `
  -H "Content-Type: application/json" `
  -d '{\"query\": \"test\", \"top_k\": 5, \"include_patterns\": [\"*.rs\"]}' | python -m json.tool

# --- Error cases ---
Write-Host "=== Error: Empty Query ==="
curl -s -X POST http://localhost:9090/search `
  -H "Content-Type: application/json" `
  -d '{\"query\": \"\"}'
# Expected: 400, "query must not be empty"

Write-Host ""
Write-Host "=== Error: Missing Query ==="
curl -s -X POST http://localhost:9090/search `
  -H "Content-Type: application/json" `
  -d '{}'
# Expected: 400, "missing required field: query"

# --- Latency benchmark ---
Write-Host "=== Latency Benchmark (10 runs) ==="
$latencies = @()
for ($i = 1; $i -le 10; $i++) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    curl -s -o NUL -X POST http://localhost:9090/search `
      -H "Content-Type: application/json" `
      -d '{\"query\": \"file system operations\", \"top_k\": 10}'
    $sw.Stop()
    $sec = $sw.Elapsed.TotalSeconds
    Write-Host "$sec"
    $latencies += $sec
}
$avg = ($latencies | Measure-Object -Average).Average
$min = ($latencies | Measure-Object -Minimum).Minimum
$max = ($latencies | Measure-Object -Maximum).Maximum
Write-Host "min=$([math]::Round($min,3))s max=$([math]::Round($max,3))s avg=$([math]::Round($avg,3))s"

# --- Reload ---
Write-Host "=== Reload ==="
curl -s -X POST http://localhost:9090/reload `
  -H "Content-Type: application/json" | python -m json.tool

# Stop daemon
Stop-Process -Id $daemon.Id -Force
Write-Host "Daemon stopped"
```

---

## Phase 6: NFS / Alternate Storage Redirect

```powershell
$PROJECT = (Get-Location).Path

New-Item -ItemType Directory -Force -Path C:\temp\colgrep-alt-store | Out-Null

# Index to alternate location
$env:XDG_DATA_HOME = "C:\temp\colgrep-alt-store"
C:\temp\colgrep-gpu.exe init -y $PROJECT 2>&1 | Tee-Object -FilePath C:\temp\index-alt.log
Remove-Item Env:\XDG_DATA_HOME

# Verify index location
Get-ChildItem -Recurse C:\temp\colgrep-alt-store\colgrep\indices\ | Format-Table Name, Length

# Search from redirected index
$env:XDG_DATA_HOME = "C:\temp\colgrep-alt-store"
C:\temp\colgrep-gpu.exe "memory allocation" $PROJECT -k 3
Remove-Item Env:\XDG_DATA_HOME

# Cleanup
Remove-Item -Recurse -Force C:\temp\colgrep-alt-store
```

---

## Phase 7: Run Unit Tests

```powershell
# All colgrep tests
cargo test -p colgrep 2>&1 | Tee-Object -FilePath C:\temp\test-results.log

# Chunked indexing tests
cargo test -p colgrep --test chunked_indexing -- --nocapture
```

---

## Phase 8: Large Repo Test (Optional)

If you have a large codebase to test with (e.g., Linux kernel, Chromium):

```powershell
# With 32GB RAM, non-chunked mode should work for most repos
C:\temp\colgrep-gpu.exe init -y C:\path\to\large\repo

# If it OOMs, use chunked mode
C:\temp\colgrep-gpu.exe init -y --chunked --chunk-files 10000 C:\path\to\large\repo

# Monitor memory: open Task Manager (Ctrl+Shift+Esc) > Performance > Memory
```

With 32GB RAM + A2000 8GB VRAM, you should handle repos up to ~200k files without chunked mode. Use `--chunked` as insurance for anything larger.

---

## Phase 9: Generate Results Report

```powershell
Write-Host "========================================="
Write-Host "  ColGREP Windows GPU Test Results"
Write-Host "========================================="
Write-Host ""
Write-Host "Date: $(Get-Date)"
Write-Host "Hostname: $env:COMPUTERNAME"
Write-Host "OS: $((Get-CimInstance Win32_OperatingSystem).Caption)"
Write-Host "RAM: $([math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)) GB"
Write-Host ""
Write-Host "--- GPU Info ---"
nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader
Write-Host ""
Write-Host "--- Binary ---"
C:\temp\colgrep-gpu.exe --version
Write-Host "Size: $([math]::Round((Get-Item C:\temp\colgrep-gpu.exe).Length / 1MB, 1)) MB"
Write-Host ""
Write-Host "--- Build Feature ---"
Write-Host "DirectML (or CUDA — check build log)"
Write-Host ""
Write-Host "--- Test Results ---"
Get-Content C:\temp\test-results.log | Select-Object -Last 5
Write-Host ""
Write-Host "========================================="
```

---

## Verification Checklist

| # | Check | Expected | Pass/Fail |
|---|-------|----------|-----------|
| 1 | `nvidia-smi` works | Shows A2000, 8GB VRAM | |
| 2 | Build with `directml` (or `cuda`) succeeds | No errors | |
| 3 | Indexing uses GPU (not CPU fallback) | Log shows GPU runtime | |
| 4 | GPU indexing faster than CPU | GPU time < CPU time | |
| 5 | `--chunked` works | Shows wave progress | |
| 6 | Daemon `/health` returns 200 | `status: ready` | |
| 7 | Daemon `/search` returns results | `search_time_ms` present | |
| 8 | Error cases return 400 | HTTP 400 on empty/missing query | |
| 9 | Alternate storage works | Index at `C:\temp\colgrep-alt-store` | |
| 10 | All unit tests pass | `cargo test` green | |
| 11 | Search latency < 1s (daemon) | avg < 1000ms | |

---

## Troubleshooting

**"CUDA support not compiled"**
Binary was built without `cuda` feature. Use `--features "cuda"` or switch to `--features "directml"`.

**"error: linker 'link.exe' not found"**
Run from "x64 Native Tools Command Prompt for VS 2022" or install Visual Studio Build Tools.

**DirectML: GPU not detected / falls back to CPU**
- Update NVIDIA driver to latest
- Ensure Windows 10 version 1903+ or Windows 11
- DirectML requires WDDM 2.7+ driver

**CUDA: "cuDNN not found"**
Copy cuDNN DLLs to CUDA bin directory:
```powershell
Copy-Item "C:\path\to\cudnn\bin\*.dll" "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6\bin\"
```

**OOM during indexing**
Use `--chunked --chunk-files 5000 --batch-size 8`. With 32GB RAM + 8GB VRAM this should never happen, but if it does, reduce batch-size further.

**Slow build**
First build downloads and compiles ONNX Runtime (~10 min). Subsequent builds are fast (~1 min).
