# Exodys

Exodys 是独立开发的 macOS 代理客户端。它使用 Rust 和 [Native Shell](https://nsh.dev/) 0.6.0 渲染原生 AppKit 界面，以本机安装的 [sing-box](https://sing-box.sagernet.org/) 驱动 HTTP 和 SOCKS5 代理。

## 使用

1. 选择 **导入配置**，粘贴 HTTP(S) 订阅地址、节点链接或配置内容，也可以打开本地文件。
2. 搜索并选择节点，然后点击 **连接**。**测试代理**会通过选中的节点向 Cloudflare 发出一次 HTTPS 请求，并显示请求耗时。
3. 在需要代理的应用中指定 `127.0.0.1:2080`。HTTP 和 SOCKS5 共用这个端口，可在 **本地端口设置**中修改。连接中可选择其他节点。每次打开应用都从断开状态开始。

导入会从 Clash YAML 的 `proxies` 或 sing-box JSON 的 `outbounds` 提取独立节点，也支持纯文本和 Base64 编码的节点链接列表。现有支持范围包括 SOCKS5、HTTP、Shadowsocks、Trojan、VLESS、VMess、Hysteria2 和 TUIC 的常用配置。协议的最终可用性取决于节点选项与本机 sing-box 版本。导入报告会显示跳过的条目数量。订阅刷新失败时保留上次成功的内容。

当前范围是 macOS 本机代理。应用级或手动设置的系统代理需要填入本地地址。TUN、自动系统代理、路由规则迁移及 Karing 备份恢复尚未实现。

配置文件存储在 `~/Library/Application Support/Exodys`。订阅地址和节点认证随配置保存在权限为 `0600` 的本地文件中，可能包含敏感信息。开发与测试可将 `EXODYS_DATA_DIR` 指向一个单独的绝对路径。

## 构建

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

## 测试

```bash
cargo test --locked
```

已安装 sing-box 时，可运行真实回环代理集成测试。测试使用独立临时目录和端口。

```bash
cargo test --locked --test proxy -- --ignored
```

运行实际 AppKit 窗口，验证配置切换、重命名、多行输入、筛选后的节点选择，以及通过窗口的 `NSToolbar` 连接、断开和退出时释放端口。此驱动使用隔离测试目录，可输出窗口截图，不需要系统级录屏或辅助功能权限。

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
