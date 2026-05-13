# hash-miner

跨平台 GPU 矿工 for [hash256.org](https://hash256.org)。Rust + wgpu，支持 Metal / Vulkan / DX12。

## 特性

- **GPU 加速** keccak256 搜索，wgpu 后端自动选最优（Metal/Vulkan/DX12）
- **自动 benchmark** 选最佳 batch_size + pipeline_depth，换 GPU 自动重测
- **动态 gas tip** 跟随市场实付分位
- **经济护栏**：实时拉 HASH/ETH/USD 价格，亏本时跳过发送
- **CLI 自管 account nonce**，并发 fire-and-continue 零撞车
- **CPU keccak 自检** 兜底 GPU 错误，防止浪费 gas
- **支持 macOS / Linux / Windows**

## 系统需求

| 平台 | GPU 后端 | 备注 |
|---|---|---|
| macOS | Metal | M1+ 推荐, Intel Mac 也可 |
| Linux | Vulkan | 需装 `vulkan-tools` + 显卡驱动 |
| Windows | Vulkan / DX12 | 显卡驱动通常自带 |

> 没有可用 GPU 也能跑（wgpu 软件回退），但速度极慢，不推荐。

## 编译

```bash
git clone <repo>
cd hash-miner
cargo build --release
# 产出: target/release/hash-miner
```

需要 Rust 1.86+。

### Linux 额外依赖

```bash
# Ubuntu/Debian
sudo apt install -y libssl-dev pkg-config vulkan-tools mesa-vulkan-drivers
# 显卡厂商驱动 (NVIDIA / AMD) 自行安装
```

## 使用

### 一键启动 (推荐)

**macOS / Linux**:
```bash
./start.sh
```

**Windows**:
```cmd
start.bat
```

脚本会自动:
1. 检查 Rust 和 GPU 驱动
2. 首次运行: 从 `config.example.toml` 生成 `config.toml` 并提示编辑
3. 增量编译 `cargo build --release`
4. 启动 `hash-miner`, 所有参数透传 (如 `./start.sh --selftest`)

### 手动启动

```bash
cp config.example.toml config.toml
# 编辑 config.toml 填私钥
cargo build --release
./target/release/hash-miner
```

首次启动会自动 benchmark 5-15 秒，选定最优 `batch_size` 和 `pipeline_depth` 写回 `config.toml`。换 GPU 后会通过 fingerprint 自动检测并重测。

### 3. 命令行参数

```
./hash-miner --help
  --config <PATH>   配置文件路径 (默认 config.toml)
  --selftest        离线自检, 验证 GPU shader 与 CPU keccak 一致
  --retune          强制重新跑 GPU benchmark
```

## 配置说明

见 [config.example.toml](config.example.toml)。主要字段：

- `private_key`：矿工私钥；留空则读 `.env` 里的 `PRIVATE_KEY`
- `[gpu]`：自动调优字段，首次启动自动写入
- `[mining].tip_mode`：`market` 跟随市场 / `fixed` 用固定 tip
- `[mining].profit_ratio`：经济护栏阈值；`1.0` = gas 超过奖励就跳过

## 终端显示

```
[算力]  200.7 MH/s · 已尝试  4.32 G nonce · 本纪元剩余 32 块 · 奖励 100.0 HASH/次
[行情] HASH=$0.2200 · base=1.72 gwei · tip=8.50 gwei · gas=0.001022 ETH ($2.54) · 奖励 $22.00 · 净 $+19.46 (11%)
```

两行原地刷新（用 ANSI escape），每秒更新一次。

### Windows 终端

PowerShell 7+ / Windows Terminal 原生支持 ANSI。老版 CMD 也能用——程序启动时会自动启用 VT100 处理。

## 跨平台编译

GitHub Actions ([release.yml](.github/workflows/release.yml)) 自动构建：

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu` (ARM Linux)
- `x86_64-apple-darwin` (Intel Mac)
- `aarch64-apple-darwin` (M1+)
- `x86_64-pc-windows-msvc`

打 tag `v*` 触发 release 构建。

## 故障排查

### `找不到可用 GPU adapter`

Linux: 装 Vulkan 驱动并验证：
```bash
vulkaninfo --summary
```

Windows: 更新显卡驱动 (NVIDIA / AMD 官网下载最新版)。

### `读取配置 ... 失败`

程序会按顺序查找 `config.toml`：
1. CLI 参数 `--config` 指定的绝对路径
2. 当前工作目录
3. 上一级目录
4. 可执行文件所在目录
5. 可执行文件的上一级目录

只要其中之一存在就行。

### `[警告] HASH/USD 价格未就绪`

DEX Screener API 偶尔抽风。等几分钟通常会恢复。或者设 `pause_when_unprofitable = false` 绕过护栏（自负盈亏）。

### gas 飙到亏损时

经济护栏自动跳过发送，GPU 继续挖。等 oracle 检测到 gas 降下来自动恢复发送。
