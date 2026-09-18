use chrono::{DateTime, FixedOffset, Utc};

/// 当前北京时间（东八区），格式 `YYYY-MM-DD HH:MM:SS`。
pub fn get_current_time() -> String {
    // 1. 获取当前的 UTC 时间
    let utc_now = Utc::now();
    // 2. 创建东八区偏移（8小时 = 28800秒）
    let china_timezone = FixedOffset::east_opt(8 * 3600).unwrap();
    // 3. 将 UTC 时间转换为东八区时间
    let china_now: DateTime<FixedOffset> = utc_now.with_timezone(&china_timezone);
    china_now.format("%Y-%m-%d %H:%M:%S").to_string()
}
