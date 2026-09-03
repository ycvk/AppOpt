# AppOptR

#### 介绍
Android 应用 CPU 亲和性管理工具 - https://gitee.com/sutoliu/AppOpt 的Rust重构版

 **使用说明请参考** 

http://appopt.suto.top

### 模块说明

| 模块 | 功能 |
|------|------|
| `main.rs` | 程序入口，CLI 解析与双模式主循环编排 |
| `config.rs` | 配置解析、inotify 热加载与降级轮询 |
| `cpuset.rs` | CPU 拓扑检测与 cpuset 管理 |
| `rule_edit.rs` | 规则匹配、规则增删改与归一化 |
| `apply_affinity.rs` | 亲和性应用 |
| `cache.rs` | 统一缓存 |
| `ebpf_mode.rs` | eBPF 事件驱动 |
| `proc_mode.rs` | Proc 轮询模式 |
| `web.rs` | 轻量 HTTP 管理面板服务 |
| `appopt-ebpf` | eBPF 内核态 |


### 请作者喝咖啡

![请作者喝奶茶](%E8%AF%B7%E4%BD%9C%E8%80%85%E6%9D%AF%E5%A5%B6%E8%8C%B6.png)

