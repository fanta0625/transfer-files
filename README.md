# Transfer Files

Rust + Slint 双栏文件传输客户端。Linux 客户端可以直连或逐级连接 Windows / Linux 中转节点，最终目标也支持 Windows / Linux。

- 多个远程标签独立配置，支持并行浏览和传输；切换标签不打断后台任务。
- 每个中转节点和目标分别设置系统类型、用户名、端口、私钥路径。
- 密码和私钥口令支持 **5 分钟过期 / 永不过期（本次程序运行期间）/ 一次性（不缓存）**，均不写入磁盘。
- 目录浏览按次连接，结束后断开并保留内存快照。
- 文件和目录通过逐段 SFTP 传输，支持校验后的断点续传、空目录、零字节文件。
- 中断后重新选择相同的源和目标，会发现已有暂存文件并续传；源内容改变时创建新任务。不会自动后台重连。
- 续传前校验已有内容的 SHA-256，传输后校验完整清单。最终文件/目录先暂存，再禁止覆盖发布；同名且内容相同跳过，不同则报错。
- 拒绝符号链接、Windows junction 和特殊文件。暂不支持控制字符文件名；跨系统文件名还受目标文件系统限制。
- 客户端仍只支持 Linux 远程操作；Windows 构建用于本地交互和 PowerShell 适配验证。

详细配置、密钥位置与续传边界见 [连接和断点续传](docs/connection-and-resume.md)。实现已加入自动化测试，实际 Windows 中转 OpenSSH 链路仍须在目标环境验收。

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

## 打包为 deb（在目标 Linux 电脑上）

安装打包工具后，在项目目录执行：

```bash
sudo apt install dpkg-dev python3
bash scripts/build-deb.sh
sudo apt install ./target/debian/transfer-files_*.deb
```

脚本自动构建 Release，读取项目版本与 `dpkg --print-architecture`，生成 `target/debian/transfer-files_版本_架构.deb`。在 Loongnix 龙芯电脑运行即可生成本机架构的包；请在相同发行版/版本和架构的电脑安装，不要把其他架构生成的包用于龙芯。

安装后从 KDE 应用菜单搜索“文件传输”或执行 `transfer-files`。项目目录可以移走，个人配置仍留在用户配置目录，不会打入安装包。更新代码后重新打包并安装即可；卸载用 `sudo apt remove transfer-files`（保留个人配置）。

若已经编译好本机程序，可省略构建：`bash scripts/build-deb.sh --binary /绝对路径/transfer-files`。打包无需 sudo。使用软件渲染，桌面入口默认 X11，与现有启动脚本一致。

## 配置远程连接

1. 点击“+ 新连接”新建标签，在“远程设置”中填写标签名称和连接配置。按实际连接顺序添加中转节点，没有中转时直接连接目标。
2. 每台电脑选择 **Linux / Windows**。旧配置自动沿用 Linux 中转、Windows 目标。
3. 为目标填写默认目录：Linux 如 `/home/user`，Windows 如 `C:/Users/user`。
4. 私钥路径可留空（使用 OpenSSH 默认密钥、agent 或密码）；指定时使用 `IdentitiesOnly=yes`。首个节点的私钥在本机，后续节点的私钥在其上一台电脑。程序不会复制私钥或转发 agent。
5. 选择密码缓存时长后保存。保存和启动均不自动连接，点击“读取远程目录”才发起连接。
6. 首次连接需要核对主机指纹，沿用发起连接端的 OpenSSH `known_hosts`。密码和私钥口令分别缓存，修改配置只清空该标签的缓存。
7. 切换标签后原任务继续，后台需要密码时标签显示“待认证”。点击 × 会取消该标签任务并移除其保存配置；退出程序会取消所有标签的任务。

客户端需要 `python3`、OpenSSH `ssh`/`sftp`。Linux 节点需要 Python 3，Windows 节点需要 Windows PowerShell 5.1。中转节点还需要 `ssh`、`sftp` 客户端，并允许 SSH 分配终端；全部被连接节点需要 OpenSSH 服务端和可用 SFTP 子系统。不使用 ProxyJump、端口或 agent 转发，不安装远端常驻服务。

Linux 多标签配置位于 `~/.config/transfer-files/profiles.json`（支持 `XDG_CONFIG_HOME`）；没有此文件时会读取旧 `config.conf` 导入第一个标签，旧文件保留。密码不写入配置、命令参数或环境变量。认证弹窗最多等待两分钟；目录读取空闲超时 60 秒；传输和大文件哈希操作空闲超时 30 分钟，取消仍会主动终止本应用的本地 SSH 子进程。

完整读取/哈希清单上限为 16 MiB，目录条目很多时请分批传输。远程进度目前按选中项目计数，不是字节进度。

未配置远程时，两栏默认打开当前目录。传入两个目录参数可强制进入本地验证模式：

```bash
cargo run --locked -- "$HOME/Downloads" "$HOME/Documents"
```

源代码更新后执行 `cargo build --release --locked`，启动脚本不会自动重建已有程序。

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
src/tabs.rs                     多标签配置持久化、独立任务与认证状态
src/config.rs                   远程连接参数的加载、校验和保存（不含密码）
src/files.rs                    本地目录读取与可取消的复制引擎
src/remote.rs                   Windows 路径处理与远程读取入口
src/remote/terminal.rs          Linux PTY、混合系统 SSH 链、认证与进程清理
src/remote/transfer.rs          逐段 SFTP、哈希校验、断点续传与发布
src/remote/operations.rs        临时文件系统操作适配（Python / PowerShell）
src/remote/fs_helper.*          Linux / Windows 文件系统操作与分帧响应
ui/app.slint                    双栏界面
docs/development-requirements.md 完整需求与后续 SSH 方案
docs/prototype-validation.md    目标机验收与当前验证记录
scripts/create-demo.sh          创建本地人工测试目录
scripts/run.sh                  构建（必要时）并启动 Release 程序
```

Slint 资料：[渲染后端](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/)、[拖放接口](https://docs.slint.dev/latest/docs/slint/reference/drag-and-drop/dragarea/)。
