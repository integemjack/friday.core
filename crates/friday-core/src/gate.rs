//! 在线引擎的请求并发闸门的规则（== Swift `LLM/Engine/RequestGate.swift`、Qt `llm/engine/RequestGate.cpp`）：不设上限，按 429 自适应——
//! 没被限流且有请求在排队就慢慢加（大约每轮 +1），被限流就减半。按引擎共用：主智能体和所有子智能体的请求都从这里过。
//! 只在请求真正在途时占名额。排队时主智能体最先，其次层级深的（先把已经开始的子树做完），同层先来先走。
//!
//! 这里只管名额、排队的先后和并发上限怎么变；排着的请求怎么等（Swift 的 continuation、Qt 的 Promise）留在两端：
//! 两端给每个排队的请求一个编号，这里每次说该放行哪几个。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// 引擎没设起步并发时从这里开始
pub const DEFAULT_INITIAL: i64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    RateLimited,
    Failed,
}

#[derive(Debug)]
pub struct Gate {
    engine_id: String,
    limit: f64,
    in_flight: i64,
    /// 排着的（编号、优先级），按来的先后
    waiters: Vec<(u64, i64)>,
    last_cut: Option<Instant>,
}

/// 一次变化的结果：该放行的编号，和要记进日志的一句话（并发上限变了时）
#[derive(Debug, Default, PartialEq)]
pub struct Change {
    pub admitted: Vec<u64>,
    pub note: Option<String>,
}

impl Gate {
    pub fn new(engine_id: &str, initial: i64) -> Self {
        Gate { engine_id: engine_id.to_string(), limit: initial.max(1) as f64, in_flight: 0, waiters: Vec::new(), last_cut: None }
    }

    fn allowed(&self) -> i64 {
        (self.limit as i64).max(1)
    }

    /// 没人排队、还有名额就直接占一个
    pub fn try_acquire(&mut self) -> bool {
        if self.waiters.is_empty() && self.in_flight < self.allowed() {
            self.in_flight += 1;
            return true;
        }
        false
    }

    /// 排队（编号由两端给，同一个闸门里不重复、越来越大）；有名额的马上放行（可能就是它自己）
    pub fn enqueue(&mut self, id: u64, priority: i64) -> Vec<u64> {
        self.waiters.push((id, priority));
        self.pump()
    }

    /// 排着的不要了（取消）：还在排就去掉；refund：已经放行却不要了，退回名额、放行下一个（Qt 的取消要这样）
    pub fn cancel(&mut self, id: u64, refund: bool) -> (bool, Vec<u64>) {
        if let Some(index) = self.waiters.iter().position(|(waiter, _)| *waiter == id) {
            self.waiters.remove(index);
            return (true, self.pump());
        }
        if refund {
            self.in_flight = (self.in_flight - 1).max(0);
        }
        (false, self.pump())
    }

    pub fn release(&mut self, outcome: Outcome, now: Instant) -> Change {
        self.in_flight = (self.in_flight - 1).max(0);
        let mut note = None;
        match outcome {
            // 一阵 429 只算一次：2 秒内不重复减半
            Outcome::RateLimited => {
                if self.last_cut.is_none_or(|cut| now - cut > Duration::from_secs(2)) {
                    let before = self.allowed();
                    self.limit = (self.limit.min((self.in_flight + 1) as f64) / 2.0).max(1.0);
                    self.last_cut = Some(now);
                    note = Some(format!(
                        "{} 被限流：并发 {before} → {}，在途 {}，排队 {}",
                        self.engine_id,
                        self.allowed(),
                        self.in_flight,
                        self.waiters.len()
                    ));
                }
            }
            // 有请求在排队（不够用）、最近 10 秒没被限流：加性增长
            Outcome::Ok => {
                if !self.waiters.is_empty() && self.last_cut.is_none_or(|cut| now - cut > Duration::from_secs(10)) {
                    let before = self.allowed();
                    self.limit += 1.0 / self.limit;
                    if self.allowed() != before {
                        note = Some(format!("{} 没被限流、有请求在排队：并发 {before} → {}", self.engine_id, self.allowed()));
                    }
                }
            }
            Outcome::Failed => {}
        }
        Change { admitted: self.pump(), note }
    }

    /// v2.8 本地大模型（friday-llm 0.2 起）在响应头 X-Friday-Parallel 里报它同时能算几个：并发上限照它——
    /// 多放的也只是在 friday-llm 里排队，卡片看不出在排队；最近 10 秒被限流过（KV 池满回 429）只往下调
    pub fn follow(&mut self, ceiling: i64, now: Instant) -> Change {
        if ceiling < 1 {
            return Change::default();
        }
        let before = self.allowed();
        let recently_cut = self.last_cut.is_some_and(|cut| now - cut <= Duration::from_secs(10));
        self.limit = if recently_cut { self.limit.min(ceiling as f64) } else { ceiling as f64 };
        let note = (self.allowed() != before)
            .then(|| format!("{} 照本地大模型报的同时能算几个：并发 {before} → {}", self.engine_id, self.allowed()));
        Change { admitted: self.pump(), note }
    }

    /// 诊断用：并发上限、在途、排队
    pub fn snapshot(&self) -> (i64, i64, usize) {
        (self.allowed(), self.in_flight, self.waiters.len())
    }

    fn pump(&mut self) -> Vec<u64> {
        let mut admitted = Vec::new();
        while self.in_flight < self.allowed() && !self.waiters.is_empty() {
            // 优先级高的先走，同优先级先来先走
            let index = (0..self.waiters.len()).min_by_key(|&index| (-self.waiters[index].1, self.waiters[index].0)).unwrap();
            self.in_flight += 1;
            admitted.push(self.waiters.remove(index).0);
        }
        admitted
    }
}

static GATES: LazyLock<Mutex<HashMap<String, Gate>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 按引擎 id 取闸门；第一次取时按 initial（None → 4，至少 1）建，之后 initial 不再起作用
pub fn with<T>(engine_id: &str, initial: Option<i64>, body: impl FnOnce(&mut Gate) -> T) -> T {
    let mut gates = GATES.lock().unwrap();
    let gate = gates.entry(engine_id.to_string()).or_insert_with(|| Gate::new(engine_id, initial.unwrap_or(DEFAULT_INITIAL)));
    body(gate)
}

/// 测试用：清掉全部闸门（引擎 id 在测试之间会复用）
pub fn reset() {
    GATES.lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapts_to_rate_limits() {
        let start = Instant::now();
        let mut gate = Gate::new("g", 4);
        assert!((0..4).all(|_| gate.try_acquire()));
        assert!(!gate.try_acquire());
        // 排队：主智能体最先，其次层级深的，同层先来先走
        assert!(gate.enqueue(1, 1).is_empty());
        assert!(gate.enqueue(2, 2).is_empty());
        assert!(gate.enqueue(3, 1_000).is_empty());
        assert!(gate.enqueue(4, 2).is_empty());
        // 被限流：减半（在途 3 + 1 → 2），2 秒内再来不重复减
        let change = gate.release(Outcome::RateLimited, start);
        assert_eq!(change.admitted, Vec::<u64>::new());
        assert_eq!(change.note.as_deref(), Some("g 被限流：并发 4 → 2，在途 3，排队 4"));
        assert_eq!(gate.release(Outcome::RateLimited, start + Duration::from_secs(1)).note, None);
        assert_eq!(gate.snapshot(), (2, 2, 4));
        // 10 秒内成功不加
        assert_eq!(gate.release(Outcome::Ok, start + Duration::from_secs(5)), Change { admitted: vec![3], note: None });
        assert_eq!(gate.snapshot(), (2, 2, 3));
        // 10 秒后成功、还有人排队：慢慢加（2 → 2.5 → 2.9 → 3.24）
        let later = start + Duration::from_secs(20);
        assert_eq!(gate.release(Outcome::Ok, later).admitted, vec![2]);
        assert_eq!(gate.release(Outcome::Ok, later).admitted, vec![4]);
        let change = gate.release(Outcome::Ok, later);
        assert_eq!(change.admitted, vec![1]);
        assert_eq!(change.note.as_deref(), Some("g 没被限流、有请求在排队：并发 2 → 3"));
        // 没人排队时成功不加
        assert_eq!(gate.release(Outcome::Ok, later).note, None);
        assert_eq!(gate.snapshot(), (3, 1, 0));
    }

    #[test]
    fn cancel_and_follow() {
        let start = Instant::now();
        let mut gate = Gate::new("g", 0);
        assert_eq!(gate.snapshot(), (1, 0, 0));
        assert!(gate.try_acquire());
        assert!(gate.enqueue(1, 1).is_empty());
        assert!(gate.enqueue(2, 1).is_empty());
        assert_eq!(gate.cancel(1, true), (true, vec![]));
        // 放行了却不要了：退回名额，放行下一个
        assert_eq!(gate.release(Outcome::Failed, start).admitted, vec![2]);
        assert_eq!(gate.cancel(2, true), (false, vec![]));
        assert_eq!(gate.snapshot(), (1, 0, 0));
        // 本地大模型报能算几个：照它；最近被限流过只往下调
        assert!(gate.try_acquire());
        assert!(gate.enqueue(3, 1).is_empty());
        let change = gate.follow(4, start);
        assert_eq!(change.admitted, vec![3]);
        assert_eq!(change.note.as_deref(), Some("g 照本地大模型报的同时能算几个：并发 1 → 4"));
        gate.release(Outcome::RateLimited, start);
        assert_eq!(gate.snapshot().0, 1);
        assert_eq!(gate.follow(8, start + Duration::from_secs(3)).note, None);
        assert_eq!(gate.follow(8, start + Duration::from_secs(11)).note.as_deref(), Some("g 照本地大模型报的同时能算几个：并发 1 → 8"));
        assert_eq!(gate.follow(0, start), Change::default());
    }

    #[test]
    fn shared_by_engine() {
        with("shared-gate-test", Some(2), |gate| assert_eq!(gate.snapshot(), (2, 0, 0)));
        with("shared-gate-test", Some(9), |gate| assert_eq!(gate.snapshot(), (2, 0, 0)));
    }
}
