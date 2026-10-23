# Transfer Files

Rust + Slint 双栏文件管理原型，为后续“公司 Linux → 公网 SSH 跳板机 → 家里 Windows”的两段传输客户端验证交互。

**已接入 Linux 客户端的两段 SSH 认证和 Windows 目录浏览。保存远程配置后，左栏为本地、右栏为远程；点击读取时连接，完成后断开并保留目录快照。远程文件和目录上传下载已实现，向 Dolphin 拖出尚未实现。**

![远程浏览原型界面](docs/images/remote-browser.png)

截图由无窗口软件渲染测试注入模拟远程结果生成，用于检查布局；不是真实 SSH 或龙芯实机截图。

## 功能

- 双栏目录浏览，路径输入、上级、刷新；目录优先、名称排序，显示大小与 UTC 修改时间。
- 勾选多项、全选、清空选择；双击目录进入。
- 本地验证模式：内部双向拖拽复制，也支持 Dolphin 拖入文件、多个文件、目录。
- 后台扫描及复制、字节进度、错误日志、取消。
- 同名目标跳过，源文件保留；目录同名整体跳过，不合并。
- “远程设置”保存非敏感参数；连接时在 GUI 分别输入两端密码，首次连接显示主机指纹确认。
- 远程模式：读取、双击目录、上级和刷新都会单独连接，完成后断开；当前目录快照保留在内存中，标注读取时间，失败时保留旧快照。
- 远程面板支持从左侧上传、从右侧下载；文件夹使用 scp -r 递归传输，Dolphin 拖入右栏也会上传。

远程传输通过两段 SCP 完成，分别使用设置中的跳板机端口和 Windows 端口。下载顺序为 Windows → 跳板机 → 本地；成功后删除本任务的跳板机暂存目录并刷新左栏。失败或取消时，日志显示失败阶段及可能保留的暂存位置，不自动删除未完成任务的数据。本地已有同名项目时下载会拒绝；上传目前沿用 SCP 的覆盖/目录合并行为。远程进度按选中项目计数，并非逐字节进度。

## 在龙芯 Linux 上运行

已由用户验证的基础环境：Loongnix 25、LoongArch64、glibc 2.41、X11、Rust/Cargo 1.95.0。此前已通过 Slint 最小窗口验证；本仓库完整原型仍需要在目标电脑验收。

依赖固定 Slint 1.18.1，使用 Winit + 软件渲染，避免依赖 Skia 预编译包或 GPU 渲染。

在 KDE 桌面的 Konsole 里，进入项目目录：

```bash
cargo run --locked
```

这是开发运行方式。首次验证完成后，可以构建一次 Release 版本，之后直接启动编译好的程序：

```bash
cargo build --release --locked
./target/release/transfer-files
```

也可以使用仓库提供的一键启动脚本。脚本只在 Release 程序不存在时构建，后续启动直接运行程序，不需要每次重新编译：

```bash
bash scripts/run.sh
```

如果希望从 KDE 应用菜单启动，可以把 `scripts/run.sh` 添加为桌面快捷方式；脚本所在的项目目录需要保留不变。源代码更新后重新执行一次 `cargo build --release --locked` 即可。

## 读取家里 Windows 的目录

### 通用连接设置（2026-10-23）

目标主机填写 IP/域名、端口、用户名和默认目录，界面自动用目标地址命名；认证弹窗显示当前节点的 `用户名@地址:端口`。

- 直连：中转节点留空，或点击“清空中转（直连）”。
- 逐级中转：点击“添加一级中转”，每行填写 `地址 端口 用户名`，从本地首先连接的节点向后排列，例如：

  ```text
  bastion.example.com 22 alice
  100.64.0.3 2222 bob
  ```

下方填写最终目标主机。编辑、删除整行或调整行顺序即可修改连接链；最多 7 级中转加 1 台目标。旧版配置自动迁移，修改配置会清空密码缓存。各节点密码按用户名、地址、端口独立缓存 5 分钟，不因其他节点输入密码延长。

当前目标目录读取仍要求 Windows PowerShell；中转节点需要 POSIX shell、ssh/scp、stty、env、mkdir、rm 和暂存空间。通用化指连接拓扑和名称，尚未支持 Linux 目标目录浏览。逐层普通 SSH 不使用 ProxyJump 或端口转发。

传输逐段暂存，全部成功后由后向前清理中转目录；直连不创建中转暂存。失败日志显示具体节点与可能残留的暂存位置。

1. 点击右上角“远程设置”，填写可选中转节点，以及目标地址、端口、用户名和默认目录，例如 `C:/Users/你的用户名`。
2. 保存后右栏切换为远程面板，点击“读取远程目录”。保存设置和启动程序都不会自动连接。
3. 若遇到新主机，核对弹窗中的主机指纹后确认。每个节点的信任记录保存在连接它的上一台电脑，本地保存首个节点的记录，沿用各端 OpenSSH 的 `known_hosts`。
4. 按提示依次输入连接链各节点的密码。密码只在内存中缓存 5 分钟，期间重复读取和传输会复用；过期后重新询问。若已有可用密钥，OpenSSH 可能直接认证成功。
5. 读完显示“SSH 已断开”。双击目录、点击上级或刷新时，会重新连接认证；当前仅缓存最近一次成功读取的目录，不在磁盘保存目录快照。

Linux 默认配置位置为 `~/.config/transfer-files/config.conf`，支持 `XDG_CONFIG_HOME`；密码不写入配置、命令参数、环境变量或日志。GUI 等待认证最多两分钟，网络读取空闲超时 60 秒，目录响应上限 16 MiB。取消或关闭窗口会请求终止本应用的 SSH 并等待清理。

前提：公司电脑和 Linux 跳板机已有 `ssh`，跳板机允许分配 SSH 终端并能运行 POSIX shell、`stty`、`env`；Windows OpenSSH 可运行 `powershell.exe`（Windows PowerShell 5.1）。无需安装跳板机辅助服务，不使用 ProxyJump、端口转发或额外隧道。当前适配普通密码、公钥及私钥口令提示；自定义 MFA/OTP、keyboard-interactive 提示尚不支持。远程浏览仅支持 Linux 客户端，Windows 构建用于本地交互和 PowerShell 协议验证。

更新源码后必须先执行 `cargo build --release --locked`，再启动程序；启动脚本不会自动重建已有程序。

未配置远程时，两栏默认打开当前目录。传入两个目录参数可强制进入本地验证模式，路径含空格时加引号：

```bash
cargo run --locked -- "$HOME/Downloads" "$HOME/Documents"
```

明确选择 X11（本应用的原生外部拖入适配针对 X11）：

```bash
WINIT_UNIX_BACKEND=x11 cargo run --locked
```

首次构建会下载 Rust 依赖。系统 C/C++ 编译工具已在用户电脑确认可用。若构建或启动提示缺少系统库，Loongnix 的 Debian 风格仓库可参考以下包名；以目标仓库实际可用包为准：

```bash
sudo apt install build-essential pkg-config libfontconfig1-dev libxkbcommon-dev libx11-dev libxcb1-dev
```

## 本地验证模式操作方式

1. 左栏打开源目录，右栏打开另一个已有目录。
2. 勾选源文件，拖动其中一个已勾选的文件行到另一栏的文件列表；选中的项目将一并复制。
3. 拖动未勾选的文件行时，仅复制该项目。
4. 右栏到左栏同样操作。也可以使用“左侧选中 → 右侧”等按钮。
5. 从 Dolphin 拖到本应用任意一栏：X11 通过落下时的窗口内坐标确定目标栏。落到栏内时，目标始终为该栏**当前目录**，不会进入悬停的子目录。
6. 若平台不提供 X11 坐标查询，则使用标注“外部拖入目标”的栏；可先点击“设为外部拖入目标”。Wayland 和其他平台的原生拖入行为未验收。
7. 复制时可以继续浏览；任务目标在发起时固定，不会随导航变化。原型同一时间只执行一批，新拖入不会追加到正在运行的批次。

原型尚不支持从 GUI 拖出到 Dolphin。内部拖拽使用应用内数据，不向外部伪装为已下载的远程文件。

## 测试目录与人工验收

可以使用脚本创建隔离的示例目录，然后按输出的命令运行：

```bash
bash scripts/create-demo.sh
```

按 [原型验收清单](docs/prototype-validation.md) 验证，尤其是 Dolphin 多文件拖入、中文输入与取消。

## 开发检查

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

测试覆盖递归复制、中文路径、空目录、零字节文件、同名跳过、目标并发出现、取消清理，以及 Slint 无窗口环境中的真实鼠标拖拽和双向复制。测试还会生成界面截图到 `target/qa/`，不进入 Git。

## 复制边界

- 不覆盖任何同名目标；同名目录不做增量合并。
- 不复制符号链接及特殊文件；不复制到源目录自身或其子目录。
- 不保留源文件权限、所有者或时间戳。
- 文件先写入目标目录中的 `.transfer-files-*.part`，完成后通过硬链接无覆盖发布；目标文件系统必须支持硬链接，例如通常的 ext4、Btrfs、NTFS。FAT/exFAT 等不支持的目标会报告错误。
- 取消时清理当前未完成文件，已完成的文件保留。正常错误路径会尝试移除空的任务目录；强制杀进程或断电可能留下 `.part`，本地原型尚未做重启恢复。
- 校验大小与修改时间，防止明显的源文件变化；没有内容级校验和，也不能保证源文件被并发修改时的一致快照。
- 目录读取及复制运行于后台线程；取消会在文件块读写之间检查，操作系统 I/O 阻塞时不会瞬间结束。

## 目录结构

```text
src/main.rs                     界面控制、目录工作线程、拖放适配、批次进度
src/config.rs                   远程连接参数的加载、校验和保存（不含密码）
src/files.rs                    本地目录读取与可取消的复制引擎
src/remote.rs                   Windows 路径处理与远程读取入口
src/remote/terminal.rs          Linux PTY、两段 SSH、认证与进程清理
src/remote/protocol.rs          PowerShell 脚本与带版本边界的目录协议
ui/app.slint                    双栏界面
docs/development-requirements.md 完整需求与后续 SSH 方案
docs/prototype-validation.md    目标机验收与当前验证记录
scripts/create-demo.sh          创建本地人工测试目录
scripts/run.sh                  构建（必要时）并启动 Release 程序
```

Slint 资料：[渲染后端](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/)、[拖放接口](https://docs.slint.dev/latest/docs/slint/reference/drag-and-drop/dragarea/)。
