//! 重试策略：指数退避 + 只重试可重试错误。

use std::future::Future;
use std::time::Duration;
use wp_core::{Error, Result};

/// 重试参数。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 额外重试次数（总尝试 = attempts + 1）
    pub attempts: u32,
    /// 首次退避（毫秒）
    pub base_ms: u64,
    /// 退避上限（毫秒）
    pub max_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            base_ms: 1000,
            max_ms: 30_000,
        }
    }
}

impl RetryPolicy {
    pub fn new(attempts: u32, base_ms: u64) -> Self {
        Self {
            attempts,
            base_ms,
            max_ms: 30_000,
        }
    }

    /// 第 `attempt` 次失败后的退避时长（attempt 从 1 开始）。
    pub fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(10);
        let ms = self.base_ms.saturating_mul(1u64 << shift).min(self.max_ms);
        Duration::from_millis(ms)
    }
}

/// 执行 `f(attempt)`，失败且可重试时按退避重试。
///
/// `f` 每次被调用拿到尝试序号（0 起），内部可据此决定是否刷新签名等。
pub async fn retry<F, Fut, T>(policy: RetryPolicy, mut f: F) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match f(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= policy.attempts || !e.retryable() {
                    return Err(e);
                }
                attempt += 1;
                let wait = policy.backoff(attempt);
                wp_core::log_debug!(
                    "网络",
                    "第 {attempt} 次重试（{} 后）：{}",
                    wait.as_millis(),
                    wp_core::error::err_line(&e)
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// 简单的可取消休眠（避免 sleep 期间无法响应取消）。
pub async fn sleep_cancellable(secs: u64, cancelled: &std::sync::atomic::AtomicBool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        if tokio::time::Instant::now() >= deadline {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 取消哨兵错误（供适配器进度回调返回 false 时使用）。
pub fn canceled() -> Error {
    Error::other("任务已取消")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn backoff_grows() {
        let p = RetryPolicy::new(3, 1000);
        assert!(p.backoff(1) < p.backoff(2));
        assert!(p.backoff(2) < p.backoff(3));
        assert_eq!(p.backoff(10), Duration::from_millis(30_000));
    }

    #[tokio::test]
    async fn retries_until_success() {
        let p = RetryPolicy {
            attempts: 3,
            base_ms: 1,
            max_ms: 2,
        };
        let n = AtomicU32::new(0);
        let r: Result<u32> = retry(p, |_| {
            let c = n.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if c < 3 {
                    Err(Error::network("暂时失败"))
                } else {
                    Ok(c)
                }
            }
        })
        .await;
        assert_eq!(r.unwrap(), 3);
    }

    #[tokio::test]
    async fn no_retry_on_auth() {
        let p = RetryPolicy {
            attempts: 3,
            base_ms: 1,
            max_ms: 2,
        };
        let n = AtomicU32::new(0);
        let r: Result<u32> = retry(p, |_| {
            n.fetch_add(1, Ordering::SeqCst);
            async { Err(Error::auth("登录失效")) }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }
}
