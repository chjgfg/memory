use crate::utils::get_current_time;
use axum::{
    extract::Path,
    http::StatusCode,
    response::Json,
};
use std::sync::Mutex;

/// 一次内存请求最多能要多少 GiB。
///
/// 上限存在的意义不是"功能需要"，而是防呆。在开启 overcommit 的 Linux 上，
/// 超额度的 try_reserve_exact 可能"虚拟地"成功（内核只登记地址空间，不立刻拒绝），
/// 随后在写实页面的循环里一路吃到被 OOM killer 干掉整个进程 ——
/// 金价播报、健康检查会一起陪葬。超限的请求在这里直接 400，一个字节都不分配。
const MAX_GIB: f64 = 6.0;

/// 1 GiB 的字节数。用 1024 进制而非 1000，是为了跟改造前的行为对齐：
/// 老代码固定占用 1536 * 1024 * 1024 字节，如今请求 1.5 拿到的字节数与之完全相同，
/// 不会出现"改完之后 1.5 反而变小了"。
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// 从没被请求过时对外报告的目标大小：1.5 GiB，即改造前的固定值。
const DEFAULT_BYTES: u64 = 1536 * 1024 * 1024;

/// 内存持有状态。
struct MemState {
    /// 当前持有的缓冲区。None 表示未持有。
    buf: Option<Vec<u8>>,
    /// 最近一次请求的字节数。和 buf.len() 的差额就是降级掉的部分。
    ///
    /// 必须单独存一份而不是复用 buf.len()：分配失败时 buf 是 None / 更小，
    /// 而 /meminfo 要如实报告"你当时要了多少"，才能一眼看出是内存不足，
    /// 而不是接口把请求值吃掉了。
    requested_bytes: u64,
}

impl MemState {
    const fn new() -> Self {
        Self {
            buf: None,
            requested_bytes: DEFAULT_BYTES,
        }
    }
}

/// 当前持有的内存缓冲区。
///
/// # 为什么必须是"存进全局静态变量"而不是局部变量
///
/// 内存能不能被"占住"，取决于它的所有权在函数返回后是否还活着。
/// 如果这里写成 `let v = 分配(...)`，函数一返回 `v` 就被 drop，
/// 那块内存立刻归还给操作系统，RSS 掉回去，等于白干。
/// 所以拿到缓冲区后必须 `guard.buf = Some(v)` 把它交给 static 的 `MEM_HOLD`，
/// 让它的生命周期等同于整个进程。
///
/// # 为什么用 Mutex 而不是 OnceLock
///
/// `OnceLock` 的语义是"一辈子只初始化一次"，天然无法释放，
/// 而这里需要能释放（置回 None）再重新分配，所以必须用 `Mutex<Option<...>>`。
/// Mutex 同时保证了多个请求并发打进来时，设定操作是串行的、状态不会错乱。
static MEM_HOLD: Mutex<MemState> = Mutex::new(MemState::new());

/// 取全局状态锁。
///
/// Mutex 中毒说明此前有线程在持锁时 panic，标准库因此把锁标记为"不可信"。
/// 但这里保护的数据只是一个 Vec<u8> 加一个 u64，不存在"改到一半的脏状态"，
/// 所以用 into_inner() 强行取回内部数据继续用，而不是让接口从此永久 500。
fn lock_state() -> std::sync::MutexGuard<'static, MemState> {
    match MEM_HOLD.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// `set_memory` 的结果。
struct HoldOutcome {
    /// 本次操作后是否持有内存。
    holding: bool,
    /// 是否发生降级：分配失败后缩量重试，或 1 MiB 都拿不到彻底放弃。
    degraded: bool,
    /// 本次请求的字节数。
    requested: u64,
    /// 实际持有的字节数；未持有时为 0。
    held: usize,
    /// 本次调用是否真的释放掉了此前持有的内存。
    /// 用于区分 /memhold/0 是一次真实释放，还是一次空转。
    released: bool,
}

/// 把持有量设定为 `target_bytes`：0 表示释放，正数表示分配并写满这么多。
///
/// 这是"设定"而不是"翻转"：重复请求同一个值会稳定维持该值，不会来回切换。
fn set_memory(target_bytes: u64) -> HoldOutcome {
    let mut guard = lock_state();

    // ── 先释放旧块，再考虑分配 ──────────────────────────────────────────
    //
    // guard.buf.take() 把旧 Vec 的所有权取出来（字段变回 None），
    // 随后显式 drop(old) 真正释放。
    //
    // 顺序不能反。若先分配新的再释放旧的，两块内存会同时存活一瞬间，
    // 峰值变成 old+new —— 请求 6G 时实际要吃 12G，容器里正好撞 OOM。
    // 先还后借，峰值始终只有一份。
    guard.requested_bytes = target_bytes;
    let released = if let Some(old) = guard.buf.take() {
        let freed = old.len();
        drop(old);
        println!("内存释放: 已释放 {} 字节 ({} MiB)", freed, freed / 1024 / 1024);
        true
    } else {
        false
    };

    // ── 请求为 0：释放完成，不重新分配 ──────────────────────────────────
    if target_bytes == 0 {
        return HoldOutcome {
            holding: false,
            degraded: false,
            requested: 0,
            held: 0,
            released,
        };
    }

    // ── 分配 ────────────────────────────────────────────────────────────
    //
    // 从请求量开始尝试；失败就减半重试（6G → 3G → 1.5G → 768M → …）。
    // 这样设计是为了让服务在内存不足的容器里也能活着：
    // 若写死请求量且失败即 panic，金价、健康检查等主功能会跟着一起挂掉，
    // 为了占内存而搭上整个服务不值得。能拿多少拿多少，实在不行优雅放弃。
    // u64 → usize。set_memory 目前只被 memhold 调用，而 memhold 已把请求限死在
    // MAX_GIB 以内，64 位平台上这里必然成立。用 try_from 而不是 `as`：
    // 万一将来放宽上限、在 32 位平台上溢出，宁可整块放弃（held: 0，调用方一眼
    // 看得出没占到），也不要静默截断成一个完全不同的大小。
    let Ok(mut want) = usize::try_from(target_bytes) else {
        println!("内存占用: 请求 {} 字节超出本机 usize 范围，放弃", target_bytes);
        return HoldOutcome {
            holding: false,
            degraded: true,
            requested: target_bytes,
            held: 0,
            released,
        };
    };
    let mut degraded = false;
    loop {
        let mut v: Vec<u8> = Vec::new();

        // try_reserve_exact 而不是 with_capacity / vec![0u8; n]：
        // 后两者在分配失败时会直接 panic 让进程崩溃；
        // try_reserve 失败只返回 Err，调用方可以决定降级策略。
        if v.try_reserve_exact(want).is_ok() {
            // 此时 v 只预留了"容量"(capacity)，但 v 自己认为"元素个数"(len) 是 0，
            // 且这块内存是未初始化的（可能残留着上一任进程留下的垃圾数据）。
            // 下面的写入需要 i < len()，所以必须先把 len 设成 want，否则会数组越界 panic。
            //
            // SAFETY: 必须自己保证"声称存在的这 want 个元素确实有效"，否则是 UB。
            // 这里成立的理由是 u8 的两个性质：
            //   1. u8 没有 Drop，析构时不会去访问内容，垃圾值也不会导致错误释放；
            //   2. u8 没有"非法位模式"，256 种取值全部合法，
            //      因此即使此刻读到垃圾数据也不会触发 UB。
            // 紧接着下面的循环会把整块内存写满，谎言立刻变成事实。
            unsafe { v.set_len(want) };

            // 这步才是真正让内存"涨"起来的地方，不能省略。
            //
            // 操作系统对内存是惰性分配的：try_reserve 只是在内核里登记了一段
            // 虚拟地址空间，一块物理内存都没给。只有真正往某个内存页写入时，
            // 内核才会发生缺页中断、分配那 4KB 物理页。
            // 若只 reserve 不写，进程 RSS 几乎不涨 —— 这正是很多"占内存"代码
            // 看起来跑了却没有效果的原因。
            //
            // 内存页大小是 4KB(4096 字节)，所以 step_by(4096) 每页碰一次就够，
            // 即使按上限要满 6G 也只要约 157 万次写入，
            // 比逐字节写 64 亿次快 4096 倍，效果完全相同。
            // 写入的值 (i & 0xFF) 本身无意义，关键在"写"这个动作。
            for i in (0..want).step_by(4096) {
                v[i] = (i & 0xFF) as u8;
            }

            // 防御性收尾：若 want 不是 4096 的整数倍，step_by 会漏掉最后一段，
            // 最后一个内存页可能没被触碰。这里补写一次，确保收尾那页也落实到物理内存。
            // 用 if let 是因为 v 可能为空，此时 last_mut() 返回 None，直接跳过而非 panic。
            if let Some(last) = v.last_mut() {
                *last = 1;
            }

            println!(
                "内存占用: 已锁定 {} 字节 ({} MiB)，请求 {} 字节",
                want,
                want / 1024 / 1024,
                target_bytes
            );
            // 把所有权交给全局静态变量一起"锁住"，函数返回后这块内存依然存活。
            guard.buf = Some(v);
            return HoldOutcome {
                holding: true,
                degraded,
                requested: target_bytes,
                held: want,
                released,
            };
        }

        // 走到这里说明 try_reserve_exact 返回了 Err —— 系统给不起这么多内存。
        // 1 MiB 都还要不到就放弃，避免拿荒谬的小块内存反复重试。
        if want <= 1024 * 1024 {
            println!("内存占用: 分配失败，放弃占用");
            return HoldOutcome {
                holding: false,
                degraded: true,
                requested: target_bytes,
                held: 0,
                released,
            };
        }

        // 减半重试，并打上降级标记。
        // 这个标记最终会出现在接口响应的 "degraded" 字段里，
        // 便于排查"为什么没占满请求量"——是内存不足，而不是代码有问题。
        want /= 2;
        degraded = true;
    }
}

/// 只读查询 (最近请求的字节数, 实际持有的字节数)，不改变状态。
fn current_state() -> (u64, usize) {
    let guard = lock_state();
    let held = guard.buf.as_ref().map(|v| v.len()).unwrap_or(0);
    (guard.requested_bytes, held)
}

/// 统一的 400 响应。所有参数错误都长同一个样，调用方只需要处理一种 JSON 结构，
/// 不用再额外判断"这次是 JSON 还是一段纯文本"。
fn bad_request(msg: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "ok": false,
            "error": msg,
            "max_gib": MAX_GIB,
            "time": get_current_time(),
        })),
    )
}

/// 内存占用设定接口：请求多少 GiB 就持有多少，传 0 释放。
///
/// ```text
/// GET /memhold/1.5   → 持有 1.5 GiB
/// GET /memhold/0     → 释放
/// GET /memhold/0.5   → 改为持有 0.5 GiB
/// ```
///
/// 这是"设定"语义而非"翻转"：重复请求同一个值只会稳定维持该值，不会来回切换。
/// 超过 MAX_GIB 一律 400，一个字节都不分配。
///
/// 注意：释放后进程 RSS 不一定会立刻下降。glibc/Windows 堆可能把这块内存
/// 留在进程内待复用，要等内核真正回收。用 /meminfo 看的是我们持有的量，
/// 不是操作系统记账的 RSS。
pub async fn memhold(Path(raw): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    // 取 String 而不是直接 Path<f64>：让"解析失败"也走 bad_request 的 JSON 错误体。
    // Path<f64> 遇到 "abc" 会被 axum 自身的 rejection 拦下，状态码虽然同样是 400，
    // 但响应体是一段纯文本 "Invalid URL: ..."，跟其余错误格式不一致。
    let Ok(gb) = raw.parse::<f64>() else {
        return bad_request(format!("gb 不是合法数字: {raw:?}"));
    };

    // 解析成 f64 之后还要挡一层语义校验。Rust 的 f64 字面量认 "NaN" / "inf"，
    // 而且 NaN 参与的所有比较都返回 false —— 包括 `gb > MAX_GIB`。
    // 不显式拦住，NaN 会一路走到 `(gb * GIB) as u64`（结果是 0，静默变成一次释放），
    // inf 则依赖平台相关的饱和转换规则。这两种"看起来像数字、行为不像数字"的输入
    // 必须在这里拒绝。
    if !gb.is_finite() || gb < 0.0 || gb > MAX_GIB {
        return bad_request(format!(
            "gb 必须是 0 ~ {MAX_GIB} 之间的数字（单位 GiB），收到 {gb}"
        ));
    }

    let target_bytes = (gb * GIB) as u64;
    let HoldOutcome {
        holding,
        degraded,
        requested,
        held,
        released,
    } = set_memory(target_bytes);

    // 四种结果要分得开，否则调用方从响应里读不出真实发生了什么：
    //   allocated —— 真的占到了
    //   failed    —— 释放掉了旧的，新的一个字节也没拿到（比 released 更值得报警）
    //   released  —— 确实释放了此前持有的内存
    //   noop      —— 请求 0 但本来就没持有，空转
    let action = if holding {
        "allocated"
    } else if target_bytes > 0 {
        "failed"
    } else if released {
        "released"
    } else {
        "noop"
    };

    let body = serde_json::json!({
        "ok": true,
        "holding": holding,
        "action": action,
        "degraded": degraded,
        "requested_gib": gb,
        "requested_bytes": requested,
        "target_bytes": requested,
        "held_bytes": held,
        "held_mib": held / 1024 / 1024,
        "time": get_current_time(),
    });
    (StatusCode::OK, Json(body))
}

/// 内存占用只读查询：不改变状态，只报告当前持有量。
///
/// `target_bytes` 是最近一次请求的量，`held_bytes` 是实际占到的量 ——
/// 两者不相等就说明发生了降级（内存不足）。
pub async fn meminfo() -> (StatusCode, Json<serde_json::Value>) {
    let (requested, held) = current_state();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "holding": held > 0,
            "target_bytes": requested,
            "held_bytes": held,
            "held_mib": held / 1024 / 1024,
            "time": get_current_time(),
        })),
    )
}
