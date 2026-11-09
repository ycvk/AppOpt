# AppOpt

Android 应用线程 CPU 亲和性优化工具。以 root 常驻运行，按 `applist.conf` 里的规则，把指定应用的进程或线程绑定到指定的 CPU 核心上，例如让渲染线程跑大核、后台推送进程留在小核。

基于 Suto 的 [AppOptR](https://gitee.com/sutoliu/AppOptR) 二次开发，遵循 GPL-3.0，见 [LICENSE](LICENSE)。

## 工作方式

**发现目标**　有两种后端：

- eBPF：内核态挂载 `sched_process_fork`、`sched_process_exec`、`sched_process_exit`、`task_rename` 四个 tracepoint，只把包名白名单命中的事件通过 RingBuf 送到用户态，进程与线程出现的瞬间就能绑定。需要内核提供 BTF（`/sys/kernel/btf/vmlinux`）。
- /proc 轮询：每个检查间隔扫描一次 `/proc`，另有周期性的全量扫描兜底。

**绑定**　先把线程写入 `/dev/cpuset/<名称>/<CPU 范围>/tasks`（cpuset cgroup v1，目录按需创建），再调用 `sched_setaffinity`。系统没有挂载 `/dev/cpuset` 时只做后一步。线程已经符合目标时不做任何写入。

**热更新**　规则文件所在目录由 inotify 监听，保存即生效，不需要重启。

## 规则

规则写在 `applist.conf`，每行一条。`#` 或 `//` 开头的行是注释，行内注释需要前面有空白。

```
# 包级规则：整个包名对应的进程
com.example=0-3

# 子进程带后缀
com.tencent.mm:push=e-core

# 单行线程规则
com.android.systemui{RenderThread}=hp-core

# 块语法：包级规则加若干线程规则，包级规则可省略
com.example=e-core,p-core {
	RenderThread=hp-core
	Worker-*=p-core
}
```

CPU 写法：

- 数字范围，如 `0-3,6`。超出在线核心的部分被裁掉。
- 语义核心：`e-core` 能效小核、`p-core` 性能中核、`hp-core` 高性能大核、`all-core` 全部核心。
- 两者可以用逗号混写，如 `e-core,p-core`、`e-core,7`。

语义核心按 cpufreq 策略分档（同一最高频率 `cpuinfo_max_freq` 的策略算一档）：

- 最高的一档是 `hp-core`。
- 其余各档里，相对性能（内核给出的 `cpu_capacity`，最强核心为 1024）不到最高一档一半的是小核，其余合并为 `p-core`。
- `e-core` 指小核。没有小核的芯片（如骁龙 8 至尊版的 6 个性能核加 2 个超大核）上，`e-core` 指最低一档，此时它也包含在 `p-core` 里。
- 内核不提供 `cpu_capacity`，或数值不低于最高一档时，按频率分层：最低一档是 `e-core`，中间各档是 `p-core`。
- 只有一档时全部算 `e-core`，`p-core` 和 `hp-core` 为空。

匹配细则：

- 线程名支持通配符 `*`、`?`、`[...]`、`[!...]`。多条线程规则同时命中时取并集。
- 线程没有命中线程规则时，使用包级规则；同一包名的多条包级规则取并集。
- 只写了线程规则、没有包级规则的包，未命中的线程恢复到全部核心。
- 内核的线程名最长 15 字节，长包名的主线程名是包名的末 15 字节，例如 `com.tencent.mobileqq` 的主线程叫 `encent.mobileqq`，规则里按这个名字写。
- 包名不超过 127 字节，线程名不超过 31 字节。

无法解析或取值无效的行被忽略，每次载入规则文件时逐行打印警告，被忽略的行数也出现在 `/api/status` 的 `parse_fail` 中。默认规则见 [module/applist.conf](module/applist.conf)。

## 运行

```
AppOpt [-c <配置文件>] [-s <秒>] [-b <名称>] [-w]
```

| 参数 | 说明 | 默认值 |
|------|------|--------|
| `-c <file>` | 规则文件路径，不存在时创建 | `./applist.conf` |
| `-s <seconds>` | 检查间隔，1-3600 秒 | `2` |
| `-b <name>` | cpuset 目录名，即 `/dev/cpuset/<name>`，不能含 `/` 与空白 | `AppOpt` |
| `-w` | 启用网页面板 | 关闭 |
| `-v` | 显示版本 | |
| `-h` | 显示帮助 | |

设置保存在工作目录下的 `AppOpt.json`，首次运行自动创建，命令行参数优先于文件，网页面板里的修改会写回文件：

| 字段 | 含义 |
|------|------|
| `web_enable` | 是否启用网页面板 |
| `mode` | `0` 自动，`1` 强制 eBPF，`2` /proc 轮询 |
| `check_interval` | 检查间隔（秒） |
| `cpuset_name` | cpuset 目录名 |
| `config_file` | 规则文件路径 |

字段缺省时取默认值，字段存在但取值无效，或文件不是合法 JSON，程序会报错退出，不会覆盖原文件。

工作模式：

- 自动：优先 eBPF，初始化失败一次后一直使用 /proc。
- 强制 eBPF：不可用时每 30 秒重试一次，期间使用 /proc。
- /proc 轮询。

### 网页面板

启用后访问 `http://127.0.0.1:8889`，可以查看运行状态、编辑规则、修改设置。服务只监听本机回环地址，并校验 `Host`、`Origin`、`Sec-Fetch-Site` 与 `Content-Type`。面板没有身份认证，本机上的任何应用都能读写规则与设置。

接口：

| 方法 | 路径 | 作用 |
|------|------|------|
| GET | `/api/status` | 运行状态、命中统计、核心分层 |
| GET | `/api/rules` | 解析后的规则，按包名分组 |
| GET | `/api/config` | 当前设置与实际生效的后端 |
| POST | `/api/config` | 修改设置 |
| POST | `/api/rule` | 新增或更新规则 |
| POST | `/api/rule/del` | 删除规则 |
| POST | `/api/rule/rename` | 重命名包名 |
| POST | `/api/suggest` | 补全建议：不带 `pkg` 时给出包名，带 `pkg` 时给出该包的线程名 |

## Magisk 模块

需要 Magisk 20.4 及以上。`module/` 是模块的全部源文件：

| 文件 | 作用 |
|------|------|
| `module.prop` | 模块信息 |
| `customize.sh` | 安装脚本：检查环境，按设备架构（arm64、x86_64）放置二进制，沿用旧版本的 `applist.conf` 与 `AppOpt.json`，并打印本机的核心分层与规则示例 |
| `service.sh` | 开机后启动 `AppOpt`，并把各 cluster 的 `core_ctl` 最小核心数调到与最大值一致 |
| `applist.conf` | 默认规则 |
| `AppOpt.json` | 默认设置，开启网页面板 |
| `webroot/index.html` | 跳转到网页面板 |
| `META-INF/` | Magisk 安装入口 |

安装后规则文件在 `/data/adb/modules/AppOpt/applist.conf`。网页面板是否启动由同目录 `AppOpt.json` 的 `web_enable` 决定，改为 `false` 后重启生效。

模块 zip 由 `module/` 的全部内容加上各架构的两个产物组成，产物放在 `bin/<架构>/` 下：

```
bin/arm64-v8a/AppOpt
bin/arm64-v8a/AppOpt-ebpf
bin/x86_64/AppOpt
bin/x86_64/AppOpt-ebpf
```

以 arm64 为例：

```bash
stage=$(mktemp -d)
cp -R module/. "$stage"
mkdir -p "$stage/bin/arm64-v8a"
cp AppOpt/target/aarch64-linux-android/release/AppOpt "$stage/bin/arm64-v8a/"
cp AppOpt-ebpf/target/bpfel-unknown-none/release/AppOpt-ebpf "$stage/bin/arm64-v8a/"
(cd "$stage" && zip -qr "$OLDPWD/AppOpt-module.zip" .)
```

## 构建

主程序需要 Rust 1.98 及以上，交叉编译到 Android 需要 NDK 的 linker：

```bash
cd AppOpt
cargo build --release --target aarch64-linux-android
```

eBPF 程序需要 nightly、`rust-src` 和 `bpf-linker`，其中 `bpf-linker` 链接的 LLVM 版本要与所用工具链的 LLVM 一致：

```bash
cd AppOpt-ebpf
cargo +nightly build --release -Z build-std=core --target bpfel-unknown-none
```

产物 `AppOpt-ebpf` 与主程序放在同一目录，程序启动时从那里加载。

测试：Linux 上 `cargo test` 运行全部测试；其他平台用 `cargo test --lib` 运行纯逻辑部分。

## 源码结构

| 目录 | 内容 |
|------|------|
| `AppOpt` | 用户态主程序 |
| `AppOpt-ebpf` | eBPF 内核态程序 |
| `AppOpt-common` | 内核态与用户态共用的数据布局 |
| `web` | 网页面板页面，编译进主程序 |
| `module` | Magisk 模块源文件 |

`AppOpt/src` 下：

| 模块 | 功能 |
|------|------|
| `main.rs` | 程序入口，装配各部分 |
| `lib.rs` | 模块声明，非 Linux 平台只编译纯逻辑模块 |
| `cli.rs` | 命令行参数 |
| `settings.rs` | 应用设置（AppOpt.json）的读写与校验 |
| `shared.rs` | 跨线程共享状态：设置、当前规则集、规则文件的重载与编辑 |
| `grammar.rs` | applist.conf 语法：逐行分类与块结构扫描 |
| `config.rs` | 规则集：按包名索引，线程规则匹配 |
| `rule_edit.rs` | 规则文件的新增、更新、删除、重命名 |
| `glob.rs` | 线程名通配匹配 |
| `cpuset.rs` | CPU 集合、核心分层、cpuset 目录与线程绑定 |
| `procfs.rs` | /proc 读取与目标进程扫描 |
| `cache.rs` | 受管线程缓存 |
| `daemon.rs` | 主循环，eBPF 与 /proc 两种后端的选择与切换 |
| `ebpf_mode.rs` | eBPF 事件驱动 |
| `proc_mode.rs` | /proc 轮询 |
| `watcher.rs` | 规则文件监听 |
| `fsutil.rs` | 原子写文件 |
| `web.rs` | 轻量 HTTP 面板服务 |
