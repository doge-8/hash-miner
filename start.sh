#!/usr/bin/env bash
# hash-miner 启动脚本 (macOS / Linux)
# 自动检查环境、编译、启动. 所有参数透传给 hash-miner.
set -e

cd "$(dirname "$0")"

# 颜色 (终端支持时)
if [ -t 1 ]; then
  C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_DIM=$'\033[2m'; C_RESET=$'\033[0m'
else
  C_RED=; C_GREEN=; C_YELLOW=; C_DIM=; C_RESET=
fi

info()  { printf "%s[i]%s %s\n" "$C_GREEN" "$C_RESET" "$1"; }
warn()  { printf "%s[!]%s %s\n" "$C_YELLOW" "$C_RESET" "$1"; }
error() { printf "%s[x]%s %s\n" "$C_RED" "$C_RESET" "$1" >&2; }

# 1. 检查 cargo
if ! command -v cargo >/dev/null 2>&1; then
  error "未检测到 Rust (cargo)"
  echo "  安装: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  echo "  装完后 source ~/.cargo/env 再运行此脚本"
  exit 1
fi
CARGO_VER=$(cargo --version | awk '{print $2}')
CARGO_MAJ=$(echo "$CARGO_VER" | cut -d. -f1)
CARGO_MIN=$(echo "$CARGO_VER" | cut -d. -f2)
if [ "$CARGO_MAJ" -lt 1 ] || { [ "$CARGO_MAJ" -eq 1 ] && [ "$CARGO_MIN" -lt 85 ]; }; then
  error "Rust 版本过旧 (cargo $CARGO_VER), 需要 1.85+"
  echo "  升级: rustup update stable"
  exit 1
fi
info "Rust 已安装: cargo $CARGO_VER"

# 2. 平台特定的 GPU 驱动检查
OS="$(uname -s)"
case "$OS" in
  Linux)
    if ! command -v vulkaninfo >/dev/null 2>&1; then
      warn "未检测到 vulkaninfo, GPU 可能不可用"
      echo "    Ubuntu/Debian: sudo apt install -y vulkan-tools mesa-vulkan-drivers"
      echo "    Arch: sudo pacman -S vulkan-tools vulkan-icd-loader"
      echo "    NVIDIA/AMD 用户还需安装对应显卡驱动"
      echo
    else
      info "Vulkan 驱动检测通过"
    fi
    ;;
  Darwin)
    info "macOS Metal 驱动系统自带"
    ;;
  *)
    warn "未知平台 $OS, 继续尝试"
    ;;
esac

# 3. config.toml 不存在则从 example 创建并提示
if [ ! -f config.toml ]; then
  if [ ! -f config.example.toml ]; then
    error "config.example.toml 也不存在, 仓库不完整?"
    exit 1
  fi
  cp config.example.toml config.toml
  chmod 600 config.toml
  warn "已生成 config.toml (权限 600)"
  echo
  echo "  ${C_DIM}请编辑填入私钥${C_RESET}:"
  echo "    ${C_GREEN}$(pwd)/config.toml${C_RESET}"
  echo
  echo "  ${C_DIM}或者在项目根目录的 .env 里设置${C_RESET}: PRIVATE_KEY=0x..."
  echo
  exit 0
fi

# 4. 编译 (cargo build --release 是增量的, 首次几分钟, 之后秒级)
info "编译 release 版本..."
cargo build --release

# 5. 启动 (透传所有参数)
echo
info "启动挖矿"
echo
exec ./target/release/hash-miner "$@"
