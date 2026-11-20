# Exodys

Exodys 是独立开发的桌面代理客户端。它使用 Rust 和 [Native Shell](https://nsh.dev/) 0.6.1 构建 macOS 和 Windows 界面，以本机安装的 [sing-box](https://sing-box.sagernet.org/) 驱动 HTTP 和 SOCKS5 代理。

[macOS 功能对齐清单](docs/karing-parity.md)区分已验证流程、支持范围和待补功能。

## 使用

1. 选择 **导入配置**，粘贴 HTTP(S) 订阅地址、节点链接或配置内容，也可以打开本地文件。
2. 搜索并选择节点，然后点击 **连接**。**测试代理**会通过本地代理按当前分流规则向 Cloudflare 发出一次 HTTPS 请求，并显示请求耗时。
3. 在需要代理的应用中指定 `127.0.0.1:2080`。HTTP 和 SOCKS5 共用这个端口，可在 **本地端口设置**中修改。连接中可选择其他节点。每次打开应用都从断开状态开始。

导入会从 Clash YAML 的 `proxies` 或 sing-box JSON 的 `outbounds` 提取独立节点，也支持纯文本和 Base64 节点链接列表，以及 Shadowsocks SIP008 v1 JSON（含常见服务器数组格式）。SIP008 中使用插件的条目会计入跳过数量。现有支持范围包括 SOCKS5、HTTP、Shadowsocks、Trojan、VLESS、VMess、Hysteria2 和 TUIC 的常用配置。协议的最终可用性取决于节点选项与本机 sing-box 版本。导入报告会显示跳过的条目数量。订阅刷新失败时保留上次成功的内容。

**macOS 系统代理（默认关闭）**：断开时打开侧栏齿轮，点 **启用系统代理模式**，再连接节点。Exodys 等待本地端口就绪后，将连接时主网络服务的 HTTP 和 HTTPS 代理指向本机端口；macOS 可能要求授权。已有启用的 HTTP/HTTPS 代理、PAC 或自动发现（WPAD）时，Exodys 不接管。拒绝授权或发现不兼容设置时，本次连接失败，不会显示已连接。更换主网络服务后请断开再连接。只影响遵循系统代理的应用，不改系统 SOCKS、DNS 或 TUN。

断开时恢复原设置；应用异常退出时，独立守护进程通过管道关闭触发恢复。守护进程也被终止或机器重启时，正常打开 Exodys 会检查本地恢复记录；也可在齿轮菜单点 **恢复系统代理**。恢复仅处理仍指向 Exodys 的设置，不覆盖用户后来更改的代理。端口／规则导出不包含此模式，导入设置不会替另一台 Mac 启用系统代理。路由规则迁移及 Karing 备份恢复尚未实现。

在侧栏齿轮打开 **分流规则**，每行填写一条规则，按顺序匹配。支持 `DOMAIN`、`DOMAIN-SUFFIX`、`DOMAIN-KEYWORD`、`IP-CIDR`、`IP-CIDR6` 和末尾的 `MATCH`。动作可选 `DIRECT`（直连）、`PROXY`（当前节点）或 `REJECT`（拒绝）。例如 `DOMAIN-SUFFIX,example.org,DIRECT` 使该域名及其子域名直连。未匹配的请求走当前节点。断开时可修改规则，下次连接生效。**导出设置** 与 **恢复设置** 通过本地 JSON 文件转移端口和分流规则。

macOS 配置存储在 `~/Library/Application Support/Exodys`，Windows 配置存储在 `%LOCALAPPDATA%\Exodys`。订阅地址和节点认证随配置保存在本地。macOS 的配置文件使用 `0600` 权限，Windows 文件继承所在目录的访问控制。开发与测试可将 `EXODYS_DATA_DIR` 指向一个单独的绝对路径。

## 构建

### macOS

在 macOS 上使用 Cargo、项目固定的 Rust 1.100.0 和 Xcode Command Line Tools。安装 sing-box 后即可实际连接。Exodys 会查找 `PATH`、`/opt/homebrew/bin` 和 `/usr/local/bin` 中的 `sing-box`，也可通过 `EXODYS_SING_BOX` 指定绝对路径。

```bash
brew install sing-box
```

```bash
cargo run --locked --release
```

本地打包脚本从 [图标源文件](assets/Exodys.png) 生成 Finder 和 Dock 使用的图标，并在 `.nsh/out/` 输出临时签名的 `.app` 和 ZIP。跨机器分发时可用 Developer ID 签名并公证。

```bash
sh scripts/package-macos.sh
```

### Windows（x64）

在 Windows 上安装 Rust 和 Visual Studio C++ 生成工具（含 Windows SDK）。将 `sing-box.exe` 所在目录加入用户 `PATH`，或设置 `EXODYS_SING_BOX` 为可执行文件的绝对路径。

```powershell
cargo build --locked --release --bin exodys
```

启动 `target\release\exodys.exe`。Windows 首版使用系统标题栏，提供配置导入与订阅刷新、节点搜索与选择、本地 HTTP/SOCKS5 代理、连接检测和分流。需要代理的应用可使用侧栏显示的地址和端口。系统代理目前由 macOS 提供。

## 测试

```bash
cargo test --locked
```

已安装 sing-box 时，可运行真实回环代理集成测试。测试使用独立临时目录和端口。

```bash
cargo test --locked --test proxy -- --ignored
```

运行实际 AppKit 窗口，验证窗口按钮的位置、配置切换、重命名、多行输入、筛选后的节点选择，以及通过侧栏连接、断开和退出时释放端口。此驱动使用隔离测试目录，可输出窗口截图，不需要系统级录屏或辅助功能权限。

```bash
cargo run --locked --example native_smoke -- .nsh/out/Exodys-preview.png
```

## 性能基线

配置侧栏与节点列表使用 AppKit `NSTableView`，搜索使用预生成的索引。操作按钮、搜索框和编辑框分别使用 `NSButton`、`NSSearchField` 和 `NSTextField`，多行导入使用滚动视图中的 `NSTextView`。NSH 负责布局和状态绑定。导入、订阅下载、磁盘写入和代理启动在后台执行。以下是此前在 Apple M1 Pro、macOS 27.2.1 上用 release 构建处理 10,000 个合成节点的数据层基线，不是本次原生控件迁移的渲染性能测量。

| 操作 | 样本 | 中位数 | P90 |
| --- | ---: | ---: | ---: |
| 导入并写入磁盘 | 15 | 43.39 ms | 58.12 ms |
| 从磁盘重新加载 | 15 | 13.61 ms | 21.72 ms |
| 筛选节点 | 75 | 0.115 ms | 0.344 ms |

```bash
cargo test --locked --release --test performance -- --ignored --nocapture
```

以上仅为 Exodys 的数据处理计时，不包含窗口渲染、首次启动、内存占用或流量转发。

## 许可证

Exodys 的源代码采用 [Apache-2.0](LICENSE) 许可。项目参考 [Karing](https://github.com/KaringX/karing) 的使用流程，独立实现界面和配置转换，未引入其源代码或美术素材。Native Shell 由 Longbridge 开发，采用 Apache-2.0 许可。sing-box 独立安装并遵循其自身许可证。
