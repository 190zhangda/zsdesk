# ZSDesk

Windows 原生多标签文件管理器与桌面格子，使用 Rust、Win32/COM、Direct2D 和 DirectWrite。

文件浏览和搜索集成在同一个主窗口，采用与桌面格子协调的界面风格。
窗口、绘图及文件操作复用 Win32、Direct2D 和 Windows Shell；共享图标、菜单及快捷键能力来自
[ZSUI](https://github.com/qiu7824/zsui)，其源码版本固定记录在发行配置中。

[下载](https://github.com/qiu7824/zsdesk/releases) · [更新日志](CHANGELOG.md) · [问题反馈](https://github.com/qiu7824/zsdesk/issues)

## 版本

当前版本：0.3.7。

| 版本 | Cargo features | 功能 |
|---|---|---|
| 标准版 | 无 | 文件管理器、搜索、预览、界面设置和系统集成 |
| 网络版 | `network` | 标准版 + FTP、SFTP/SSH 地址、网络驱动器和网络位置 |
| 桌面版 | `desktop` | 标准版 + 桌面格子与腾讯布局导入 |
| 全量版 | `network,desktop` | 全部功能，默认构建版本 |
| 主题版 | `themed` | 全量功能 + 亚克力、Mica、纯色深浅主题，独立安装与配置 |
| 桌面盒子独立版 | `desktop-box` | 默认启动桌面格子，独立配置与自启，不包含文件管理窗口、全盘搜索和接管 |

打包脚本支持为上述六个版本生成 Windows x64 便携 ZIP 和安装器，文件名为
`ZSDesk-<版本>-<standard|network|desktop|full|themed|desktop-box>-x64`。
安装器默认按当前用户安装，保留个人数据，文件夹接管和登录启动均由软件内设置。

## 文件管理

- 多标签、收藏栏、地址面包屑和系统快速访问。
- 标签栏可置于上方、左侧或右侧；中键点击文件夹打开新标签，双击标签可关闭，标签数量可隐藏。
- 左侧使用原生 Shell 导航树，支持展开、收起、固定位置和系统虚拟文件夹。
- 文件列表使用 `IExplorerBrowser`，文件操作、拖放、冲突处理和进度由 Windows Shell 执行。
- 文件夹、回收站、此电脑、网络及控制面板使用系统显示名称。
- 界面遵循系统缩放；Windows 11 22H2 及以上支持 Mica 和系统窗口按钮。

“≡ → 界面设置”可控制整排命令栏以及新建、剪切、复制、粘贴、重命名、共享、删除、
排序、查看、更多和预览的显示。隐藏项目后自动重排，窄窗口使用紧凑布局。

“双栏布局”可在界面设置或右下角按钮中开启、关闭。两侧保留各自的标签、目录、选区和搜索条件，
点击一侧后，地址栏、命令栏和快捷键操作该侧；拖动中间分隔条调整宽度。
右下角同时保留详细信息与缩略图切换按钮。关闭某栏最后一个标签时保留另一栏，
关闭窗口最后一个标签时默认关闭文件窗口，可在界面设置中改回保留空标签。

界面设置还提供回收站、网络导航项的显隐，以及“此电脑”启动时展开、折叠或跟随起始位置。
列标题随视图切换，仅在详细信息视图显示。鼠标手势可开关：按住右键向左为后退、向右为前进、
向上为上一级、向下为刷新；原地右键仍打开原生菜单。
“≡ → 新建独立窗口”或 `Ctrl+N` 打开另一个文件窗口。

| 快捷键 | 功能 |
|---|---|
| Ctrl+T / Ctrl+W | 新建 / 关闭标签 |
| Ctrl+L / Ctrl+F | 地址输入 / 快速搜索 |
| Alt+← / Alt+→ / Alt+↑ | 后退 / 前进 / 上一级文件夹 |
| 鼠标后侧键 / 前侧键 | 后退 / 前进文件夹 |
| Ctrl+C / Ctrl+X / Ctrl+V | 复制 / 剪切 / 粘贴文件 |
| Ctrl+Z / Ctrl+Y | 撤销 / 重做 Windows Shell 支持的文件操作 |
| Ctrl+A | 全选文件 |
| Ctrl+Shift+A | 反向选择文件 |
| F2 / Delete / Shift+Delete | 重命名 / 删除 / 永久删除 |
| F5 | 刷新当前目录或搜索 |
| Alt+P / 空格 | 预览窗格 / 快速预览 |
| Ctrl+N / F11 | 新窗口 / 全屏切换 |
| F6 / Shift+F6 | 下一个 / 上一个输入区域 |
| Ctrl+滚轮 / Ctrl++ / Ctrl+- / Ctrl+0 | 图标缩放 / 放大 / 缩小 / 恢复默认大小 |
| Ctrl+Shift+1～8 | 切换图标、列表、详细信息、平铺等视图 |

输入框和文件重命名编辑框保留文本编辑快捷键；隐藏右键菜单项目不会禁用文件快捷键。
普通删除遵循系统回收站规则；永久删除、不可回收位置及系统未记录的操作不能撤销。

## 搜索与预览

搜索框固定在地址栏右侧，`Ctrl+F` 聚焦输入文件名或 Everything 搜索表达式，例如 `ext:pdf`。
结果直接显示在当前标签页的文件列表中，不展开独立搜索面板。清空输入或按 `Esc` 返回原文件夹。
“命令栏 … → 搜索选项”提供文件类型、搜索范围、排序、上一页、下一页及索引设置。
每页显示最多 200 项，状态栏显示总数和当前页数量；不同标签页分别保留搜索条件。

**无需单独安装 Everything 客户端。** 标准版、网络版、桌面版、全量版和主题版随包提供
官方 Everything 1.4.1.1032 x64 非 Lite 引擎，未找到可复用实例时在后台启动随包引擎。
也可以在“≡ → 界面设置 → Everything 搜索 → 选择已安装的 Everything…”中指定自己的程序。
ZSDesk 通过 Unicode IPC 查询文件名索引；普通文件列表查询仅请求结果路径，
不再启动自研磁盘索引或维护另一份 SQLite 文件名数据库。

快速全盘 NTFS 索引需要兼容的 Everything 服务或管理员权限。未安装服务时，可在
“≡ → 界面设置 → Everything 搜索 → 启用快速索引服务…”中授权启用 ZSDesk 专用服务。
普通权限下既没有兼容服务，也没有已配置可用索引时，搜索会提示设置；不会静默退回到遍历所有磁盘。
索引正在加载或建立时显示进度，就绪后自动执行查询，未就绪状态不会当作零结果。
索引范围、包含和排除规则由 Everything 管理，
未纳入其索引的位置不会出现在搜索结果中。
“≡ → 界面设置 → Everything 搜索 → 显示文件夹大小”默认关闭。
开启后，普通文件夹的详细信息视图按需读取 Everything 已索引的文件夹大小；
需要引擎启用文件夹大小索引，未提供的大小保持为空，不启动目录扫描，也不修改文件属性。
两栏分别更新大小，关闭开关恢复原生显示；搜索结果、库及不支持大小缓存的虚拟视图保留原生表现。
Everything 保留原始程序和许可，位于 `engines/everything/`；发行包不包含 `.ini`、`.db` 或个人搜索数据。
只复制 `ZSDesk.exe` 而遗漏引擎目录，且没有可复用的 Everything 时，文件名搜索不可用；
需要保留完整便携目录或修复安装。桌面盒子独立版不提供文件名搜索，也不附带 Everything。

“≡ → 开机启动与搜索快捷键”分别设置主窗口、后台快捷键和桌面格子的登录启动；全局快捷键默认为 `Ctrl+Alt+Space`。
快捷键激活主文件管理窗口并聚焦搜索框，不创建独立搜索窗口。
全局快捷键冲突会显示错误并保留设置入口；安装包默认不启用登录启动，索引服务由用户授权启用。

同一配置的文件窗口、桌面格子、快捷键与搜索请求管理共用一个 ZSdesk 进程，文件窗口关闭后复用原有 UI 线程。
桌面图标恢复监护单独运行，桌面线程或主进程异常结束时恢复系统图标；Office 预览宿主仅在预览期间运行。
文件位置转发辅助组件与 Everything 索引引擎按各自功能运行，关闭搜索不会退出用户原有的 Everything。

右侧预览窗格、空格及悬停预览使用独立预览宿主，优先调用系统预览处理器，
可选择系统默认、Office 优先或 WPS 优先，支持已注册的 32 位及 64 位处理器，并提供文本和图片回退。
组件缺失、关联失效或被系统禁用时显示具体状态，不更改系统文件关联，也不把解码器加载到主窗口进程。
Office 文档缺少可用插件时，可用已安装 WPS 只读渲染为临时 PDF；文档关闭后释放组件和临时文件。
预览宿主异常或加载超时不会阻塞文件管理窗口。
悬停预览默认关闭，可在界面设置中开启；停留后才显示，拖动和文字输入时不触发，不改变文件选区。
文本、图片尺寸及保留图像有边界，关闭预览释放文件流和显示资源。

## 右键菜单

主文件窗口和桌面格子的文件菜单在独立宿主中加载与执行，支持经典及可用的 Windows 11 样式。
空闲宿主有界复用，当前选区和命令在每次请求中重新核对；宿主异常会返回状态。
“≡ → 诊断日志…”打开独立的只读日志窗口，显示菜单阶段、异常代码与冷暖启动耗时，支持刷新和复制。
日志保留当前及上一代，仅保存于本机。

启用“默认用 ZSDesk 打开文件夹和磁盘”后，`Win+E` 同时打开 ZSDesk；程序未运行时会自动启动。
入口使用当前用户的 Explorer 启动命令；后台运行时提供 Win+E 兼容接管。
桌面“此电脑”具有独立的默认打开关联；原值及类型随接管设置备份，停用时恢复。

主窗口的 Windows 11 模式由原生 Shell 视图处理文件右键；经典模式和格子内文件菜单使用原生
Shell 菜单宿主。文件菜单不再先显示自定义常用命令菜单，7-Zip、WPS 等命令依赖本机已注册的扩展。
格子的文件菜单在临时隔离进程中执行，加载超时或扩展异常会返回错误，主窗口与格子仍可继续使用。
“≡ → 右键菜单设置”控制菜单样式和可见命令；格子读取相同的显示设置。

格子标题、右上角更多按钮和空白处打开格子管理菜单，提供锁定、重命名、主题、标签与启动设置。
其中“文件夹系统菜单…”打开所在文件夹的原生背景菜单；“所选项目”保留移动到格子等管理操作。
格子管理命令与 Windows 文件命令分别派发，避免命令编号冲突和点击错项。

支持的 Win11 扩展过滤在加载前执行。原生菜单兼容层校验系统组件版本，
未支持的系统使用原版菜单；过滤失败时保留隐藏设置并显示错误状态。
这些设置仅作用于 ZSDesk 内的菜单。

鼠标侧键分别向后、向前浏览文件夹；地址输入和文件重命名期间保留编辑状态。
鼠标驱动同时发送侧键与浏览命令时只导航一次。
复制、剪切先保存有界文件路径快照，粘贴和移动在独立操作队列中执行，保留系统进度、冲突处理、取消和撤销。

## Listary 快速切换

Listary 插件通过官方 FileAppPlugin 接口提供当前标签页已打开的文件夹。
插件源码和部署步骤见 [native/listary](native/listary/README.md)。启用后，在标准打开/保存对话框中
按 Listary 的 `Ctrl+G` 快速切换快捷键，可切换到当前 ZSDesk 文件夹。
搜索结果、虚拟系统位置和尚未完成的导航不作为快速切换目录。

## 桌面格子

桌面版和全量版可从主菜单启动格子，或执行 `ZSDesk.exe --desktop`。
桌面盒子独立版直接启动格子，配置保存在 `%APPDATA%\ZSDesk-DesktopBox`；安装标识和登录启动与其他版本独立。

- 文件夹映射、桌面项目分组、手动排序、大小和屏幕位置。
- 悬停临时展开，移开收起；双击固定展开，支持锁定和拖动。
- 多标签：点标题栏「+」或把一个格子的标题拖到另一个格子的标题栏合并；点击、滚轮或 Ctrl+Tab 切换，
  拖动标签排序，拖出标题栏成为独立格子。标签共用位置、大小、折叠和锁定，关闭标签不删除文件。
- 图标/列表视图、主题、毛玻璃、透明度、托盘及桌面空白双击开关。
- 标题文件数量默认隐藏，可在格子“外观”设置中启用。
- 一键整理使用项目引用；删除格子不会删除文件。
- 整理模式配有恢复进程，异常退出时恢复系统桌面图标。
- 图标请求、冷缓存与绘图资源按可见状态回收。

腾讯桌面布局迁移只读取本机布局文件，保留分组、成员顺序和屏幕布局，
不移动原文件、不修改腾讯配置，也不分发腾讯程序或图形资源。

```powershell
# 预览迁移报告；“-”表示自动查找本机腾讯布局。
ZSDesk.exe --import-tencent - --import-report "C:\Reports\layout.json"

# 退出 ZSDesk 格子后应用迁移；原配置保留为 desktop-fences.before-tencent.json。
ZSDesk.exe --import-tencent - --import-report "C:\Reports\layout.json" --apply-import
```

## 文件夹与应用接入

“≡ → 系统集成”提供不接管、增加右键入口、默认打开文件夹和磁盘，以及按备份恢复。
操作仅涉及当前用户注册表；“此电脑”可由 ZSDesk 打开，其他系统虚拟文件夹保持原有默认处理方式。
“接管新出现的系统浏览窗口”可转发新打开的单标签文件夹与“此电脑”，确认目标位置及选区后才关闭来源窗口。
已有窗口、多标签窗口、状态已改变的窗口及文件打开/保存对话框不会被关闭。

“系统集成 → 文件位置转发”可以发现和选择 Codex、微信及其他兼容的 x64 应用，
也可手动添加程序，然后一键启用所选应用。登录启动单独设置，默认关闭。
仅调用受支持 Windows 文件定位接口的应用可接入；程序身份和调用模块在本地校验。
应用升级或转发组件被占用时会提示重新发现或重启相应应用。

组件由源码构建，随安装包放在 `routing/`。默认不带应用绑定或个人路径；
启用时才在当前配置目录生成绑定。已有旧版配置可保留兼容转发。
多个配置目录使用独立窗口标识，避免将文件定位请求送到另一实例。
支持严格识别的系统 `explorer.exe` 文件导航进程启动；未知开关、调试或挂起启动等仍由系统处理。
少数程序通过辅助 DLL 打开文件夹，可在“补充程序组件”中明确选择该程序目录内的调用组件。
已加载旧组件的应用需要先关闭，再应用设置，最后重新打开；旧转发保持可用，不会强制结束应用。
组件和配置说明见 [native/routing/README.md](native/routing/README.md)。

网络版和全量版支持系统 FTP、SMB/UNC，以及映射/断开网络驱动器、添加网络位置。
`sftp://` 和 `ssh://` 地址需要另行安装 SSHFS-Win 与 WinFsp。

## 数据

默认目录为 `%APPDATA%\ZSDesk`。程序旁的 `portable.flag` 使用相邻 `data/` 目录，
`--profile-dir` 可指定独立配置目录。
主题版默认使用 `%APPDATA%\ZSDesk-Theme`，安装目录、启动标识和配置与其他版本独立。
主题版可在“主题与材质”中切换亚克力、Mica、纯色浅色和纯色深色；不支持材质效果的系统使用纯色背景。
地址栏输入 `pwsh`、`powershell`、`nu` 或 `nushell` 可在当前文件夹启动对应终端。

桌面布局、会话、搜索索引、应用绑定和系统接管备份仅保存于本机，
不包含在源代码、默认配置或发行包中。卸载保留个人数据，并仅恢复仍由当前安装拥有的系统集成。

## 功能边界

- 文件列表、文件操作和虚拟目录复用 Windows Shell；本程序不替换系统任务栏、Shell 服务或其他应用的打开/保存对话框。
- 标签页快照保存视图类型、图标大小、可见列顺序及宽度、排序、分组、焦点、滚动位置及最多 256 项选择（PIDL 总量 128 KiB）；不支持相应接口的虚拟位置保留原生默认行为。
- 全盘搜索使用 Everything 的文件名及元数据索引，不维护文件内容索引；实际覆盖范围和可用搜索语法取决于连接的 Everything 实例及其索引设置。
- 应用接入覆盖可识别的文件定位接口，不承诺接管每个软件的全部打开方式；SFTP/SSH 依赖 SSHFS-Win 与 WinFsp。
- Windows 11 原生菜单及预览能力受系统版本和已安装扩展影响，未支持的菜单组件使用原版表现形式。

## 构建

需要 Windows x64、Rust MSVC 工具链、Visual Studio C++ 工具与 Windows SDK；
验证工具链为 Rust 1.99.0。打包使用 PowerShell 7，安装器需要 Inno Setup 6.3 或更新版本。
构建需要 Rust 1.89 或更新版本；持续集成使用 Rust 1.99。
VC Runtime 从 Visual Studio x64 Redist 目录获取。
Everything 使用固定版本的官方便携分发文件，来源与 SHA-256 记录在
`packaging/engines/everything.json`。`-EverythingDir` 指向包含原始程序、语言包、完整许可及该清单的目录。

ZSUI 按需启用原生窗口、虚拟列表、输入和快捷键等功能。其固定源码版本记录于
`packaging/zsui-revision.txt`，位于本仓库的同级 `zsui/` 目录。

```powershell
git clone https://github.com/qiu7824/zsdesk.git
git clone https://github.com/qiu7824/zsui.git
git -C zsui checkout (Get-Content zsdesk/packaging/zsui-revision.txt -Raw).Trim()
Set-Location zsdesk

$env:CARGO_TARGET_DIR = 'C:\Build\ZSDesk-target'
$env:CARGO_PROFILE_DEV_DEBUG = '0'
$env:CARGO_PROFILE_TEST_DEBUG = '0'
cargo test --workspace --release --locked
cargo build --release --locked -p zsdesk-app

# 可选功能版本
cargo build --release --locked -p zsdesk-app --no-default-features
cargo build --release --locked -p zsdesk-app --no-default-features --features network
cargo build --release --locked -p zsdesk-app --no-default-features --features desktop
cargo build --release --locked -p zsdesk-app --no-default-features --features themed
cargo build --release --locked -p zsdesk-app --no-default-features --features desktop-box

# 转发组件与六个发行版本
./native/routing/build.ps1 -OutputDir 'C:\Build\ZSDesk-routing' -BuildDir 'C:\Build\ZSDesk-routing-build'
./scripts/package.ps1 -WorkDir 'C:\Build\ZSDesk-package' -TargetDir $env:CARGO_TARGET_DIR `
    -RoutingDir 'C:\Build\ZSDesk-routing' -EverythingDir 'C:\Build\Everything-1.4.1.1032-x64' `
    -InnoCompiler 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe'
```

打包器验证程序版本与 feature 身份，生成文件哈希清单，移除本机编译路径信息；
已有发行目录不会被静默覆盖。`-Editions full,desktop` 可选择部分版本，
`-RuntimeDir` 可指定 Visual Studio Redist 下的 CRT 目录。
打包器逐项校验 Everything 的官方文件哈希与资源版本，仅复制固定的四个分发文件。

项目代码使用 [MIT](LICENSE)；ZSUI 使用 MPL-2.0，MinHook 使用 BSD-2-Clause。
Everything 及其 PCRE 组件的版权和完整许可见
[Everything-1.4.1.1032-License.txt](packaging/licenses/Everything-1.4.1.1032-License.txt)。
其余组件声明见 [packaging/licenses/NOTICE.txt](packaging/licenses/NOTICE.txt)。
