# macOS TUN 服务

TUN 使用 Exodys 当前选中的节点或自动组、分流规则引用的策略组，以及已安装的 `.srs` 规则集。管理员部署服务后，为每份要使用的配置批准一个版本。已经批准的版本可以重复使用。

## 准备配置

在 Exodys 选好节点或自动组并保存分流规则，然后退出应用。用同一普通 macOS 账户准备配置。将示例地址换成物理网络可达的 DNS 服务器 IP。

```bash
cargo run --locked --release -- --prepare-tun-config 192.0.2.53
```

命令用本机 sing-box 校验配置，将 DNS 保存在此 Mac 的应用数据目录，并输出配置编号和私有文件夹的位置。文件夹中的 `config.json` 可能包含明文节点认证信息。请在本机交给管理员审核。批准后可以删除这份候选副本。

批准后打开 Exodys，在侧栏齿轮菜单点 **连接 TUN**。服务报告引擎就绪后，窗口显示「运行中 · TUN」及所选节点或自动组。点 **断开 TUN** 或工具栏的 **断开** 会等待服务停止引擎。关闭窗口会释放服务连接。

切换节点前先断开 TUN。节点、策略组、分流、引用的规则集内容或 DNS 改变时，连接会请求对应版本。如果管理员尚未批准该版本，应用会显示所需的配置编号。可以提前批准多个常用配置。

## 管理员部署

在独立的 macOS 测试机或虚拟机准备以下文件。构建命令以普通用户身份运行。

```bash
cargo build --locked --release --bin exodys-tun-service
```

安装根目录为 `/Library/PrivilegedHelperTools/io.github.bernadeliot-sudo.exodys.tun/`，权限为 `root:wheel`、`0700`。服务检查路径各级的归属和权限，拒绝符号链接和扩展 ACL。

| 安装根目录中的文件或目录 | 内容 | 权限 |
| --- | --- | --- |
| `exodys-tun-service` | 上一步构建的二进制 | `root:wheel`，`0755` |
| `sing-box` | 从可信来源取得并核对发行摘要的 sing-box 1.14.4 二进制 | `root:wheel`，`0755` |
| `uid` | 使用 Exodys 的普通 macOS 账户的十进制 UID | `root:wheel`，`0600` |
| `approved` | 已批准版本的父目录 | `root:wheel`，`0700` |
| `approved/<配置编号>/` | 每个版本的私有目录 | `root:wheel`，`0700` |
| `approved/<配置编号>/config.json` 和引用的 `.srs` | 从候选目录审核并复制的文件 | `root:wheel`，`0600` |

请先把候选文件复制成 root 私有目录中的新文件，再审核该副本中的配置、出站选项及规则集。只批准依赖同目录 `.srs` 的配置。保持文件原始字节，并在审核完成后将整个目录移至 `approved/<配置编号>/`。已批准版本按原样保留，更新时发布新编号。停用某个编号前先断开使用它的连接。服务每次连接时都会核对配置及所引用 `.srs` 的完整性，随后从该目录启动预装的引擎。

将 [launchd 模板](../native/io.github.bernadeliot-sudo.exodys.tun.plist)中的 `Sockets.Client.SockPathOwner` 改为 `uid` 文件中的同一数值。确认模板路径仍指向受保护的服务二进制。将已配置的 plist 以 `root:wheel`、`0644` 安装到 `/Library/LaunchDaemons/io.github.bernadeliot-sudo.exodys.tun.plist`。使用下面的命令注册后，Exodys 才能请求启动服务。

```bash
sudo launchctl bootstrap system /Library/LaunchDaemons/io.github.bernadeliot-sudo.exodys.tun.plist
```

有需要时先在 App 内断开，然后用下面的命令停用并注销服务。

```bash
sudo launchctl bootout system/io.github.bernadeliot-sudo.exodys.tun
```

此前部署过固定 `config.json` 的测试版本时，需要同时更新应用和服务，并按上述步骤批准所需版本。

## 验收范围

本地测试使用合成节点和真实 sing-box 校验当前策略生成的 TUN 配置，使用本机 Unix 套接字和回环引擎验证请求、缺少批准、文件被修改、启动确认及进程回收。真实隧道需要在已部署的独立 macOS 上验证虚拟网卡创建、IPv4 和 IPv6 的 TCP 与 UDP 流量、DNS、物理网卡直连和网络切换，并检查断开、启动失败和应用异常退出后的路由与 DNS 恢复。
