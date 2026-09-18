mod api;
mod utils;
use axum::{routing::get, Router};
use std::net::{SocketAddr, SocketAddrV4};

#[tokio::main]
async fn main() {
    // 路由注册
    let app: Router = Router::new()
        // 内存占用设定：请求多少 GiB 就持有多少，传 0 释放，上限 6 GiB。
        // 路径参数用 axum 0.7 的 ":gb" 语法；0.8 起才改成 "{gb}"，这里别跟着改。
        .route("/memhold/:gb", get(api::memhold))
        // 内存占用查询接口：只读，不改变状态
        .route("/meminfo", get(api::meminfo));

    // 读取HF环境端口，本地默认8085
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8085".to_string())
        .parse::<u16>()
        .expect("端口解析失败");

    // 0.0.0.0 监听所有网卡，公网可访问
    let addr: SocketAddr = SocketAddr::V4(SocketAddrV4::new([0, 0, 0, 0].into(), port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("绑定 {addr} 失败: {e}"));
    println!("服务启动: http://{addr}");
    println!("占用内存: http://{addr}/memhold/1.5  (换成任意 0~6 的数字，传 0 释放)");
    println!("查看内存: http://{addr}/meminfo");
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}
