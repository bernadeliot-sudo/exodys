# Exodys

Exodys 是独立开发的桌面代理客户端。它使用 Rust 和 [Native Shell](https://nsh.dev/) 0.6.1 构建 macOS 和 Windows 界面，以本机安装的 [sing-box](https://sing-box.sagernet.org/) 驱动 HTTP 和 SOCKS5 代理。

[macOS 功能对齐清单](docs/karing-parity.md)区分已验证流程、支持范围和待补功能。

## 使用

1. 选择 **导入配置**，粘贴 HTTP(S) 订阅地址、节点链接或配置内容，也可以打开本地文件。
2. 搜索并选择节点，然后点击 **连接**。**测试代理**会通过本地代理按当前分流规则向 Cloudflare 发出一次 HTTPS 请求，并显示请求耗时。
3. 在需要代理的应用中指定 `127.0.0.1:2080`。HTTP 和 SOCKS5 共用这个端口，可在 **本地端口设置**中修改。手动模式下，连接中可选择其他节点。每次打开应用都从断开状态开始。

导入会从 Clash YAML 的 `proxies` 或 sing-box JSON 的 `outbounds` 提取独立节点，也支持纯文本和 Base64 节点链接列表，以及 Shadowsocks SIP008 v1 JSON（含常见服务器数组格式）。SIP008 中使用插件的条目会计入跳过数量。现有支持范围包括 SOCKS5、HTTP、Shadowsocks、Trojan、VLESS、VMess、Hysteria2 和 TUIC 的常用配置。协议的最终可用性取决于节点选项与本机 sing-box 版本。导入报告会显示跳过的条目数量。订阅刷新失败时保留上次成功的内容。侧栏齿轮可开启 **定时更新**（默认关闭）：App 打开时按各订阅的周期检查，跳过正在使用的配置，一次更新一条。默认周期为 24 小时，可在每条订阅的 **更新周期** 中设置 1 至 720 小时，填 0 则只手动刷新。失败后本次运行中至少一小时才自动重试。没有时间记录、且允许定时更新的旧订阅会在启用后排队。

**跨配置自动选择**：断开时在各配置的节点列表里选中节点，点击 **将所选节点加入自动组**；从侧栏打开 **自动选择组** 并启用自动模式。最多可加入 32 个节点。连接后 sing-box 的 URLTest 按探测结果在组内选择出口，活跃时每分钟检查一次，探测会经过组内节点访问 `https://www.cloudflare.com/cdn-cgi/trace`。故障切换影响后续连接，不迁移已建立的连接。回到手动模式不会清空组，也不会改变原有手动选择。

**独立策略组**：先在节点列表选择节点，再打开侧栏 **策略组**，创建或选择组并点 **加入所选节点**。每组最多 32 个成员，可指定一个手动出口，或开启自动选择。分流中可用 `DOMAIN,example.org,GROUP:work`，也可用 `MATCH,GROUP:work` 指定默认规则。组在被规则引用时才会加载。组内自动选择使用同一 Cloudflare 探测地址。若引用的组失去出口，连接会提示修复，避免流量改走其他节点。设置 JSON 只保存规则，恢复前需创建规则引用的同名策略组。

**macOS 系统代理（默认关闭）**：断开时打开侧栏齿轮，点 **启用系统代理模式**，再连接节点。Exodys 等待本地端口就绪后，将连接时主网络服务的 HTTP 和 HTTPS 代理指向本机端口；macOS 可能要求授权。已有启用的 HTTP/HTTPS 代理、PAC 或自动发现（WPAD）时，Exodys 不接管。拒绝授权或发现不兼容设置时，本次连接失败，不会显示已连接。更换主网络服务后请断开再连接。只影响遵循系统代理的应用，不改系统 SOCKS、DNS 或 TUN。

断开系统代理时恢复原设置；应用异常退出时，独立守护进程通过管道关闭触发恢复。守护进程也被终止或机器重启时，正常打开 Exodys 会检查本地恢复记录；也可在齿轮菜单点 **恢复系统代理**。恢复仅处理仍指向 Exodys 的设置，不覆盖用户后来更改的代理。恢复设置或完整备份时，会保留目标 Mac 的系统代理模式。

**TUN**：为当前节点或自动组、分流规则及引用的规则集生成配置，由管理员批准对应版本。部署服务后，从侧栏齿轮选 **连接 TUN**。切换节点前请先断开。点 **断开 TUN** 或关闭窗口会释放服务连接。[配置准备、部署与验收](docs/tun-macos.md)

在侧栏齿轮打开 **分流规则**，每行填写一条规则，按顺序匹配。支持 `DOMAIN`、`DOMAIN-SUFFIX`、`DOMAIN-KEYWORD`、`IP-CIDR`、`IP-CIDR6` 和末尾的 `MATCH`。动作可选 `DIRECT`（直连）、`PROXY`（按当前模式代理）或 `REJECT`（拒绝）。例如 `DOMAIN-SUFFIX,example.org,DIRECT` 使该域名及其子域名直连。未匹配的请求也按当前模式代理。断开时可修改规则，下次连接生效。**从 YAML 载入草稿** 只读取本地 Clash 文件的 `rules` 列表，保存前不会生效。CIDR 项要求 `no-resolve`；遇到策略组、远程规则集或其他不支持的语法时整份草稿不替换。**导出设置** 与 **恢复设置** 通过本地 JSON 文件转移端口和分流规则。

在侧栏齿轮打开 **规则集**，可选择本地 sing-box `.srs` 文件或输入 HTTPS 地址。导入时校验并保存副本。已安装的规则集可以手动替换，也可用 **保存定时来源** 保存其 HTTPS 地址。应用打开且断开时会检查更新，每次成功更新后间隔 24 小时；失败时保留原副本，本次运行中一小时后才自动重试。断开时可立即更新或停止定时更新。先安装再在分流规则中引用，例如 `RULE-SET,my-set,DIRECT`、`GEOIP,cn,PROXY`（引用 `geoip-cn`）或 `GEOSITE,cn,REJECT`（引用 `geosite-cn`）。Geo 数据由你选择导入。GeoIP 匹配请求中的 IP 地址，域名请求不会为匹配 GeoIP 而额外解析。移除规则集前，先保存一份没有该引用的分流规则。设置 JSON 只携带引用名称，恢复前需安装同名规则集。

**备份与同步**：在侧栏齿轮打开 **备份与同步**，选择 **完整导出…**，保存 `.exodysbackup` 并设置至少 12 个字符的口令。完整备份包含节点与认证信息、订阅、策略组、端口、分流规则、已安装规则集及其更新来源。选择 **完整恢复…**，确认替换后输入同一口令。恢复要求断开连接，备份内含规则集时还需要本机 sing-box 校验。

完整备份可用于单次迁移。恢复会替换目标配置，已有配置的文件仍保留在该 Mac 的应用数据目录。

**共享文件夹自动同步**：在 **备份与同步 → 自动同步** 中，为每台 Mac 选择同一个可访问的共享文件夹并输入相同口令。断开代理且停留在节点列表、自动选择组或同步页面时，App 每分钟检查一次。每次启动需重新解锁，口令只保留在本次运行的内存中。节点选择和系统代理开关按设备保留。不同配置项的改动会自动合并。同一配置项出现冲突时，可选择保留此设备或采用共享版本，选中的整份配置作为结果，双方旧版本仍留在历史中。加密的同步文件以 `.exodyssync` 结尾，文件夹最多保留 512 个版本，达到上限后可选择新文件夹继续同步。

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

启动 `target\release\exodys.exe`。Windows 首版使用系统标题栏，提供配置导入与订阅刷新、节点搜索与选择、本地 HTTP/SOCKS5 代理、连接检测和分流。需要代理的应用可使用侧栏显示的地址和端口。系统代理、TUN 与备份同步界面目前在 macOS 提供。

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
