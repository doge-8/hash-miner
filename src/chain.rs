use alloy::{
    consensus::Transaction as _,
    eips::BlockNumberOrTag,
    network::EthereumWallet,
    primitives::{Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};
use anyhow::{Context, Result};
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract Hash256 {
        function mine(uint256 nonce) external;
        function getChallenge(address miner) external view returns (bytes32);
        function currentDifficulty() external view returns (uint256);
        function currentReward() external view returns (uint256);
        function epochBlocksLeft() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
    }

    #[allow(missing_docs)]
    #[sol(rpc)]
    contract ChainlinkAggregator {
        function latestAnswer() external view returns (int256);
        function decimals() external view returns (uint8);
    }
}

/// Chainlink ETH/USD 主网预言机地址
pub const CHAINLINK_ETH_USD: &str = "0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419";

#[derive(Clone)]
pub struct ChainCtx {
    pub miner: Address,
    pub contract: Address,
    pub read_rpc: String,
    pub submit_rpc: String,
    pub signer: PrivateKeySigner,
    pub submit_timeout_secs: u64,
    pub wait_confirmations: u64,
    /// CLI 自己管的 account nonce 池, fetch_add 保证并发安全.
    pub next_nonce: Arc<AtomicU64>,
}

pub struct EpochSnapshot {
    pub challenge: [u8; 32],
    pub difficulty: [u8; 32],
    pub reward: U256,
    pub blocks_left: U256,
}

pub enum SubmitOutcome {
    Confirmed {
        tx_hash: B256,
        block: u64,
        gas_used: u64,
    },
    Reverted {
        tx_hash: B256,
        reason: String,
    },
    /// 超时未确认, 链上 challenge 已变 (被自己或他人推进了).
    TimeoutChallengeChanged {
        tx_hash: B256,
    },
    /// 超时未确认, 链上 challenge 未变, tx 大概率被丢弃.
    TimeoutDropped {
        tx_hash: B256,
    },
}

impl ChainCtx {
    pub fn new(
        private_key: &str,
        contract: &str,
        read_rpc: String,
        submit_rpc: String,
        submit_timeout_secs: u64,
        wait_confirmations: u64,
    ) -> Result<Self> {
        let signer = PrivateKeySigner::from_str(private_key.trim_start_matches("0x"))
            .or_else(|_| PrivateKeySigner::from_str(private_key))
            .context("解析私钥失败")?;
        let miner = signer.address();
        let contract = Address::from_str(contract).context("解析合约地址失败")?;
        Ok(Self {
            miner,
            contract,
            read_rpc,
            submit_rpc,
            signer,
            submit_timeout_secs,
            wait_confirmations,
            next_nonce: Arc::new(AtomicU64::new(0)),
        })
    }

    /// 启动时同步链上 pending nonce, 初始化本地 nonce 池.
    pub async fn init_nonce(&self) -> Result<u64> {
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let n = provider
            .get_transaction_count(self.miner)
            .pending()
            .await?;
        self.next_nonce.store(n, Ordering::SeqCst);
        Ok(n)
    }

    /// 拉一次 pending nonce, 取 max(本地, 链上) 写回本地池.
    /// 取 max 防止 store 一个比已分配的小的值导致后续撞车.
    pub async fn resync_nonce(&self) -> Result<u64> {
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let on_chain = provider
            .get_transaction_count(self.miner)
            .pending()
            .await?;
        let cur = self.next_nonce.load(Ordering::SeqCst);
        let target = on_chain.max(cur);
        self.next_nonce.store(target, Ordering::SeqCst);
        Ok(target)
    }

    pub async fn snapshot(&self) -> Result<EpochSnapshot> {
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let c = Hash256::new(self.contract, &provider);
        let challenge = c.getChallenge(self.miner).call().await?._0;
        let difficulty = c.currentDifficulty().call().await?._0;
        let reward = c.currentReward().call().await?._0;
        let blocks_left = c.epochBlocksLeft().call().await?._0;
        Ok(EpochSnapshot {
            challenge: challenge.0,
            difficulty: difficulty.to_be_bytes::<32>(),
            reward,
            blocks_left,
        })
    }

    pub async fn balance(&self) -> Result<U256> {
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let c = Hash256::new(self.contract, &provider);
        Ok(c.balanceOf(self.miner).call().await?._0)
    }

    pub async fn fetch_challenge(&self) -> Result<[u8; 32]> {
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let c = Hash256::new(self.contract, &provider);
        Ok(c.getChallenge(self.miner).call().await?._0.0)
    }

    /// 同时读 challenge + 剩余块, 减少 RPC 调用数.
    pub async fn fetch_challenge_and_blocks_left(&self) -> Result<([u8; 32], u64)> {
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let c = Hash256::new(self.contract, &provider);
        let challenge = c.getChallenge(self.miner).call().await?._0.0;
        let blocks_left = c.epochBlocksLeft().call().await?._0;
        let blocks_u64 = blocks_left.try_into().unwrap_or(u64::MAX);
        Ok((challenge, blocks_u64))
    }

    /// 读当前 era 的奖励 (HASH per mint, 已经除过 1e18).
    pub async fn fetch_reward_hash(&self) -> Result<f64> {
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let c = Hash256::new(self.contract, &provider);
        let r = c.currentReward().call().await?._0;
        // U256 -> f64. 转 u128 然后除 1e18
        let raw: u128 = r.try_into().unwrap_or(0);
        Ok((raw as f64) / 1e18)
    }

    /// 读最新区块的 base fee (wei).
    pub async fn fetch_base_fee(&self) -> Result<u128> {
        use alloy::eips::BlockNumberOrTag;
        use alloy::providers::Provider;
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let block = provider
            .get_block_by_number(BlockNumberOrTag::Latest, false.into())
            .await?
            .ok_or_else(|| anyhow::anyhow!("无法获取最新区块"))?;
        Ok(block.header.base_fee_per_gas.unwrap_or(0) as u128)
    }

    /// 扫最近 `n_blocks` 个区块的成功 mine() tx, 取实付 priority fee 的指定分位.
    /// 返回 wei. 没有样本时返回 None.
    pub async fn fetch_market_tip(
        &self,
        n_blocks: u64,
        percentile: f64,
    ) -> Result<Option<u128>> {
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let latest = provider.get_block_number().await?;
        // mine(uint256) selector
        const MINE_SELECTOR: [u8; 4] = [0x4d, 0x47, 0x48, 0x98];

        let mut tips: Vec<u128> = Vec::new();
        for offset in 0..n_blocks {
            let blk_num = latest.saturating_sub(offset);
            let block = match provider
                .get_block_by_number(BlockNumberOrTag::Number(blk_num), true.into())
                .await?
            {
                Some(b) => b,
                None => continue,
            };
            let base_fee = block.header.base_fee_per_gas.unwrap_or(0) as u128;
            let txs = match &block.transactions {
                alloy::rpc::types::BlockTransactions::Full(t) => t,
                _ => continue,
            };
            for tx in txs {
                if tx.inner.to() != Some(self.contract) {
                    continue;
                }
                let data = tx.inner.input();
                if data.len() < 4 || data[..4] != MINE_SELECTOR {
                    continue;
                }
                // 拿 receipt 确认成功 + 实付 gas price
                if let Ok(Some(rcpt)) =
                    provider.get_transaction_receipt(*tx.inner.tx_hash()).await
                {
                    if rcpt.status() {
                        let eff = rcpt.effective_gas_price as u128;
                        if eff > base_fee {
                            tips.push(eff - base_fee);
                        }
                    }
                }
            }
        }
        if tips.is_empty() {
            return Ok(None);
        }
        tips.sort_unstable();
        let idx = ((tips.len() as f64 - 1.0) * (percentile / 100.0)).round() as usize;
        Ok(Some(tips[idx.min(tips.len() - 1)]))
    }

    /// 通过 DEX Screener API 拉 HASH 实时价格 (USD).
    pub async fn fetch_hash_usd(&self) -> Result<f64> {
        let url = format!(
            "https://api.dexscreener.com/latest/dex/tokens/{}",
            self.contract
        );
        let resp: serde_json::Value = reqwest::Client::new()
            .get(&url)
            .timeout(Duration::from_secs(8))
            .send()
            .await?
            .json()
            .await?;
        // pairs[0].priceUsd 是最高流动性的池子
        let price = resp
            .get("pairs")
            .and_then(|p| p.get(0))
            .and_then(|p| p.get("priceUsd"))
            .and_then(|p| p.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .ok_or_else(|| anyhow::anyhow!("DEX Screener 未返回有效 priceUsd"))?;
        Ok(price)
    }

    /// 读 Chainlink ETH/USD 价格, 返回 USD/ETH (f64).
    pub async fn fetch_eth_usd(&self) -> Result<f64> {
        use std::str::FromStr;
        let provider = ProviderBuilder::new().on_http(self.read_rpc.parse()?);
        let oracle = ChainlinkAggregator::new(Address::from_str(CHAINLINK_ETH_USD)?, &provider);
        let answer = oracle.latestAnswer().call().await?._0;
        // Chainlink ETH/USD 默认 8 位小数
        let raw: i128 = answer.try_into().unwrap_or(0);
        if raw <= 0 {
            return Err(anyhow::anyhow!("Chainlink 返回非正价格"));
        }
        Ok((raw as f64) / 1e8)
    }

    /// 发送 mine(nonce) tx, 用 CLI 自管的 account nonce + 显式 tip.
    /// 失败时调用方应该 resync_nonce().
    pub async fn submit_mine(
        &self,
        nonce: u64,
        challenge_before: [u8; 32],
        tip_gwei: f64,
    ) -> Result<SubmitOutcome> {
        let wallet = EthereumWallet::from(self.signer.clone());
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .on_http(self.submit_rpc.parse()?);
        let c = Hash256::new(self.contract, &provider);

        let tip_wei = (tip_gwei * 1e9) as u128;
        let account_nonce = self.next_nonce.fetch_add(1, Ordering::SeqCst);
        let pending = c
            .mine(U256::from(nonce))
            .nonce(account_nonce)
            .max_priority_fee_per_gas(tip_wei)
            .send()
            .await
            .context("send mine() tx 失败")?;
        let tx_hash = *pending.tx_hash();

        let confirmed = pending
            .with_required_confirmations(self.wait_confirmations)
            .with_timeout(Some(Duration::from_secs(self.submit_timeout_secs)));

        match confirmed.get_receipt().await {
            Ok(receipt) => {
                if receipt.status() {
                    Ok(SubmitOutcome::Confirmed {
                        tx_hash,
                        block: receipt.block_number.unwrap_or(0),
                        gas_used: receipt.gas_used as u64,
                    })
                } else {
                    Ok(SubmitOutcome::Reverted {
                        tx_hash,
                        reason: "合约 revert (可能是 nonce 失效或难度已变)".into(),
                    })
                }
            }
            Err(e) => {
                // 超时或其他错误. 用 challenge 对比判定 tx 实际状态.
                let now = self.fetch_challenge().await?;
                if now != challenge_before {
                    Ok(SubmitOutcome::TimeoutChallengeChanged { tx_hash })
                } else {
                    let _ = e;
                    Ok(SubmitOutcome::TimeoutDropped { tx_hash })
                }
            }
        }
    }
}

pub fn format_hex_short(bytes: &[u8]) -> String {
    let h = hex::encode(bytes);
    if h.len() < 12 {
        format!("0x{}", h)
    } else {
        format!("0x{}…{}", &h[..8], &h[h.len() - 6..])
    }
}

pub fn format_hash_18(amount: U256) -> String {
    let s = amount.to_string();
    if s.len() <= 18 {
        let pad = "0".repeat(18 - s.len());
        format!("0.{}{}", pad, s.trim_end_matches('0'))
    } else {
        let split = s.len() - 18;
        let (int_part, frac) = s.split_at(split);
        let frac = frac.trim_end_matches('0');
        if frac.is_empty() {
            int_part.to_string()
        } else {
            format!("{}.{}", int_part, frac)
        }
    }
}

