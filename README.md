# ftpseg — 多线程分段 FTP/FTPS 下载客户端

[![CI](https://github.com/example/ftpseg/actions/workflows/ci.yml/badge.svg)](.github/workflows/ci.yml)

用 Rust 编写的跨平台 FTP/FTPS 客户端，提供命令行和本机浏览器 GUI；核心能力是**多线程分段并行下载**：把远端文件切成若干字节区间，用多条连接同时抓取，每个区间单独用 `REST` 定位后 `RETR`，并直接按绝对偏移写入本地文件。中断后可以从断点继续，不必重下已经拿到的部分。

```
$ ftpseg get ftp://ftp.example.com/pub/big.iso -n 8 --limit 20M
✓ 1.42 GiB/1.42 GiB  24.31 MiB/s eta 0s  done in 61.3s
saved big.iso — 1.42 GiB in 61.3s (23.74 MiB/s)
16 segment(s) over 8 connection(s)
```

## 特性

- **分段并行下载** — 连接池 + 工作窃取队列，慢连接不会拖住结尾；连接数可配，段大小可固定或自动推导。
- **真正的断点续传** — 每段已写字节数记录在 `<文件>.ftpseg-state.json`，定时原子落盘；进程被杀死或 Ctrl-C 之后重跑同一条命令即可续传。
- **显式与隐式 FTPS** — `AUTH TLS`（RFC 4217）升级，数据连接用 `PROT P` 加密；`ftps://` 等价于 990 端口的隐式 TLS。
- **能力探测与自动降级** — 服务器不支持 `REST` 时自动退回单连接；不支持 `SIZE` 时从 `MLSD`/`LIST` 里取大小；不支持 `EPSV` 时退回 `PASV`；不支持 `MLSD` 时退回 `LIST`。
- **可靠性细节** — 段级重试与指数退避；被提前关闭的数据连接用 `NOOP` 重新对齐控制通道，因此连接可以安全复用；`426`/`426+226`/无应答三种服务器行为都能正确处理。
- **限速** — `--limit` 是全局令牌桶，不是每条连接各自限速。
- **进度** — 总进度条 + 每条连接的独立进度条，实时速率与 ETA；非终端或 `-q` 时自动静默。
- **字符集处理** — 服务器通告 `UTF8` 就用 UTF-8，否则回退到本机代码页（中文 Windows 即 936/GBK）；`--encoding gbk` 等可强制指定。中文文件名与中文账号密码都能正确转换，目录列表也按同一字符集解码。

## 构建

需要 Rust 1.75 或更高版本。

### macOS

```sh
rustup toolchain install stable
cargo build --release
cargo test
```

Intel 与 Apple Silicon 都可以；如需通用二进制：

```sh
rustup target add x86_64-apple-darwin aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
lipo -create -output ftpseg \
  target/x86_64-apple-darwin/release/ftpseg \
  target/aarch64-apple-darwin/release/ftpseg
```

### Windows

两条路，任选其一。

**A. 标准 rustup + MSVC（推荐，如果机器上有 Visual Studio）**

装好「使用 C++ 的桌面开发」工作负载后：

```powershell
rustup default stable-msvc
cargo build --release
cargo test
```

**B. 零依赖：工作区自带工具链（不需要 Visual Studio，但需要 Python 3）**

```powershell
# 一次性安装：Rust 工具链 + 便携版 MinGW-w64，全部落在工作区的 .rust\ 下
.\scripts\install-toolchain.ps1

# 之后每次构建
.\scripts\cargo.ps1 build --release
.\scripts\cargo.ps1 test
```

产物位置：命令行 `target/release/ftpseg`，GUI 快捷启动器 `target/release/ftpseg-gui`（Windows 下均为 `.exe`）。

#### 为什么 Windows 上还需要一个 MinGW

Rust 的 `x86_64-pc-windows-gnu` 套件自带 `gcc`、`ld`、`dlltool`，但**没有汇编器 `as`**。而 tokio、clap、native-tls 等依赖通过 `raw-dylib` 链接 Windows API，rustc 会调用 `dlltool` 现造导入库，`dlltool` 又必须调用 `as` 才能把生成的汇编变成目标文件——于是链接阶段会失败：

```
error: dlltool could not create import library ... CreateProcess
```

`scripts\fetch-mingw.py` 会下载 w64devkit（约 64 MiB，便携版 MinGW-w64）解到 `.rust\mingw\`，`scripts\env.ps1` 把它的 `bin` 目录放到 `PATH` 最前面，`as` 与一整套 `gcc` 就都齐了。

#### 另外两个 Windows 环境细节

- `scripts\env.ps1` 会把 `TMP`/`TEMP`/`TMPDIR` 指向工作区内的 `.tmp\`。MinGW 链接器生成 proc-macro DLL 时需要可写的临时目录，而机器级的 `%LOCALAPPDATA%\Temp` 在部分机器上对当前账户不可写（本机就是这种情况，表现为 `Cannot create temporary file in ...: Permission denied`）。
- `scripts\env.ps1` 还会把 rustup 的 `self-contained` 目录加进 `PATH`，因为 rustc 是按名字找 `dlltool` 的。

如果只想手动来，等价于：

```powershell
$root = (Get-Location).Path
$env:RUSTUP_HOME = "$root\.rust\rustup"; $env:CARGO_HOME = "$root\.rust\cargo"
$env:CARGO_TARGET_DIR = "$root\target"
$env:TMP = $env:TEMP = $root + "\.tmp"; New-Item -ItemType Directory -Force $env:TMP

# 顺序很重要：self-contained 必须在 MinGW 之前，rustc 才会选它做链接器并带上
# 自己的库目录（libgcc_eh.a 在那里）；而 as 只存在于 MinGW 里。
$selfContained = "$root\.rust\rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"
$env:PATH = "$root\.rust\cargo\bin;$selfContained;$root\.rust\mingw\w64devkit\bin;$env:PATH"

cargo build --release
```

## 图形界面

**免命令行启动：**构建后直接双击 Windows 的 `target/release/ftpseg-gui.exe`，或在 macOS/Linux 的文件管理器中运行 `target/release/ftpseg-gui`。该启动程序会启动本机 GUI 并打开默认浏览器；重复打开只会打开同一页面，不会另启下载服务。不再创建系统托盘图标。页面右上角「退出应用」可在没有下载中、待下载或目录扫描任务时安全关闭服务，随后前端切换为「应用已关闭」。浏览器关掉标签页不会停止后台下载；重复双击启动程序可重新打开界面。Windows 启动器读取 HTTP 默认浏览器关联命令，安全解析参数后直接启动该浏览器，不再创建 Explorer 子进程；无法解析直接启动命令时备用调用初始化 COM 的 `ShellExecuteExW`，不依赖 `cmd /C start`；不显示命令提示符窗口，出错时弹出具体错误及手动访问地址。若无法自动打开，请检查 Windows「设置 → 应用 → 默认应用」中的 HTTP 浏览器关联。程序目录下 `.ftpseg/gui.json` 保存站点信息，请勿将该文件分享给其他人。

```sh
ftpseg gui                 # 启动 GUI，在浏览器访问 http://127.0.0.1:17721/
ftpseg gui --port 17722    # 指定本机监听端口
```

使用浏览器访问终端显示的地址；服务仅监听本机回环地址，不对局域网开放。Windows 使用 `scripts/cargo.ps1 run -- gui`，macOS/Linux 使用 `cargo run -- gui`。无需 Electron 或额外 Node 运行时，FTP/FTPS 的网络传输仍由现有 Rust 引擎完成。启动后可新建、编辑和删除站点，浏览目录，勾选多个文件或目录加入队列（目录递归扫描并保留子目录结构），调整同时下载的任务数和单文件连接数，并查看各文件下载进度、失败原因及重试。下载列表支持“全部/正在下载/待下载/已完成/失败或取消”分类，正在下载的任务优先显示；面板汇总已知总大小、累计已下载量及实时下载速度。远端文件和下载列表都可折叠，收起后仍显示动态汇总。下载成功后仍支持原有断点续传逻辑。**后端任务队列仅保存在内存中**；浏览器 localStorage 缓存任务列表（含远端路径、本地保存路径和进度，不保存账号密码），页面重开先展示缓存并自动向后端核查状态。同一次后端运行的任务以服务器结果为准；后端重启后旧缓存只作为只读历史，不自动重建下载或操作复用编号的新任务。不同浏览器/端口缓存不共享，清除浏览器数据会丢失缓存；缓存异常不影响下载。退出后端后需重新加入未完成任务；单文件任务使用 `<任务编号>-<原文件名>` 独立存储；目录下载使用独立批次目录保留选中目录及子目录结构，避免同名文件并发覆盖。目录与文件混选、重叠目录会按远端路径去重；扫描失败不添加部分任务。空目录不生成下载任务；扫描限制为最多 10000 个文件/目录、64 层深度，防止异常服务端无限递归。下载队列支持勾选多个任务、全选当前分类及批量暂停/继续/取消/重试；批量操作栏默认隐藏，选中任务后才显示，并高亮选中项，可一键清空选择。切换分类会清空选择，避免操作隐藏任务；后台刷新保留仍有效的选择。不适用的状态自动跳过，批量取消会统一确认。下载中任务点「暂停」先停止写入并保存续传记录，等待任务可直接暂停；暂停完成后点「继续」重新排队，支持分段续传（不支持 REST 的服务器可能从头传输）。暂停保留文件，与取消删除文件不同；暂停处理中不能继续或退出，但可改为取消。后端退出后暂停任务仅在浏览器保留历史显示，不自动恢复队列。失败任务未经清理时可断点重试。GUI 点击「取消」会先停止全部写入，再删除未完成目标文件、续传记录及其临时文件，显示「正在停止并清理文件…」；取消后重试从头开始。失败任务也可用「清理文件」删除残留；删除失败会显示原因并可再次清理。已完成任务文件不删除，父目录不递归清理。CLI 取消仍保留原有断点续传行为。新站点配置优先写入**当前运行程序所在目录**的 `.ftpseg/gui.json`（如 `target/release/.ftpseg/gui.json`），与启动命令所在的工作目录无关；若已有旧版配置，则继续使用 Windows `%APPDATA%\ftpseg\gui.json` 或 macOS/Linux `${XDG_CONFIG_HOME:-~/.config}/ftpseg/gui.json`，避免站点突然消失。程序目录不可写时依次尝试当前工作目录下的 `.ftpseg/gui.json` 和系统配置目录。终端会打印实际使用的路径，也可用 `FTPSEG_GUI_CONFIG` 指定路径；**账号和密码使用便携轻量混淆保存**（`obfuscated:v1`，固定密钥 XOR + 十六进制编码，附损坏校验），不再绑定 DPAPI 用户或系统钥匙串，可跨账户/机器复制配置。**这不是安全加密，拥有配置和程序的人可以还原凭据**；请勿分享配置。旧版明文配置自动迁移；旧 DPAPI/钥匙串配置仅在原环境可解密时自动转换，无法解密则报错且保留旧文件，请先以原账户运行新版完成迁移，或重新配置站点。历史备份不会被自动清除。密码不会回传给页面；Unix 新建文件权限为 `0600`，生产环境建议优先使用 FTPS（本地混淆不保护 FTP 网络明文传输）。

### Windows 发布签名与安全提示

目前本地构建产物没有 Authenticode 数字签名，因此 Windows 可能显示「未知发布者」或 SmartScreen 提示。这与打开浏览器的程序逻辑不同；若程序被系统阻止，它的代码还未执行。正式分发需要发布者提供受信任的代码签名证书（或企业部署的可信证书）并用 SHA-256 签名、时间戳服务签署发布文件；自签证书或修改文件描述不能消除公共机器上的发布者信任问题，已签名文件也不保证立即获得 SmartScreen 信誉。不建议关闭 Defender/SmartScreen 或绕过企业安全策略；仅在确认文件来源可信且组织允许时，按 Windows 提示处理。程序启动后若浏览器没有打开，可直接访问 `http://127.0.0.1:17721/`。

## 用法

```
ftpseg [全局选项] <子命令>
```

### 子命令

| 子命令 | 说明 |
| --- | --- |
| `get <目标>` | 分段并行下载一个文件 |
| `ls [-l] <目标>` | 列出远端目录（优先 `MLSD`，否则 `LIST`） |
| `stat <目标>` | 显示文件大小、修改时间与服务器能力（`FEAT`） |
| `gui [--port 17721]` | 启动跨平台本机图形界面 |

`<目标>` 可以是完整的 `ftp://` / `ftps://` URL，也可以是配合 `--host` 使用的裸路径。

### 全局选项

| 选项 | 说明 |
| --- | --- |
| `-u, --user <USER>` | 用户名，默认 `anonymous`（也可用 `FTPSEG_USER`） |
| `-p, --password <PASS>` | 密码；建议用 `FTPSEG_PASSWORD` 或写进 URL，避免进入 shell 历史 |
| `-P, --port <PORT>` | 控制端口，默认 `21`（隐式 FTPS 为 `990`） |
| `--tls` | 显式 FTPS：明文连接后 `AUTH TLS` 升级 |
| `--tls-implicit` | 隐式 FTPS：首字节即 TLS（`ftps://` 同义） |
| `--insecure` | 不校验服务器证书 |
| `--timeout <SECONDS>` | 回复超时，默认 30 |
| `--connect-timeout <SECONDS>` | 连接超时，默认 15 |
| `--host <HOST>` | 目标为裸路径时指定主机 |
| `--no-prompt` | 不交互询问密码，缺失即报错 |
| `--encoding <CHARSET>` | 远端路径与账号密码的字符集，默认 `auto`（见下文「字符集与账号密码」） |
| `--threads <N>` | 运行时工作线程数，默认等于 CPU 核数 |
| `-v, --verbose` | 打印完整的 FTP 对话 |

### `get` 选项

| 选项 | 说明 |
| --- | --- |
| `-o, --output <PATH>` | 保存位置；给目录则沿用远端文件名 |
| `-n, --connections <N>` | 并行连接数，默认 4 |
| `-s, --segment-size <SIZE>` | 固定段大小，如 `8M`；默认按文件大小推导 |
| `--min-segment <SIZE>` | 段的最小值，默认 `1M` |
| `--limit <RATE>` | 全局速率上限，如 `2M`、`500K` |
| `--retries <N>` | 每段重试次数，默认 3 |
| `--no-resume` | 忽略已有断点记录，但仍保留现有文件内容 |
| `--force` | 删除目标文件，彻底重下 |
| `--keep-state` | 成功后保留断点文件 |
| `-q, --quiet` | 不显示进度 |
| `--dry-run` | 只打印分段计划，不传输 |

`SIZE` 与 `RATE` 支持 `K`/`M`/`G`/`T` 后缀（1024 进制），可写 `8M`、`64KB`、`2GiB`。

### 示例

```bash
# 匿名下载，8 条连接
ftpseg get ftp://ftp.example.com/pub/debian.iso -n 8

# 带凭据、限速 20 MiB/s、存到指定目录
ftpseg get ftp://example.com/big.tar.gz -u alice -o /data/ --limit 20M

# 显式 FTPS
ftpseg get ftp://secure.example.com/secret.zip --tls -u alice

# 隐式 FTPS（等价于 ftps://）
ftpseg get ftps://secure.example.com:990/secret.zip -u alice --insecure

# 先看计划：文件多大、能不能分段、会切成几段
ftpseg get ftp://ftp.example.com/pub/debian.iso --dry-run

# 看服务器支持哪些能力
ftpseg stat ftp://ftp.example.com/pub/debian.iso

# 列目录
ftpseg ls -l ftp://ftp.example.com/pub/

# 断点续传：中断后重跑同一条命令即可
ftpseg get ftp://ftp.example.com/pub/debian.iso -n 8
```

## 工作原理

一次下载分四步：

1. **探测** — 用一条连接登录，取 `SIZE`（失败则从父目录的 `MLSD`/`LIST` 里找该条目的 size）、`MDTM`、`FEAT`，并用 `REST 0` 实际验证服务器是否真的支持字节区间（只看 `FEAT` 里的 `REST STREAM` 不够）。
2. **切分** — 默认按 `连接数 × 4` 段来切（下限 `--min-segment`），这让负载能自然均衡；段数上限 512，避免断点文件过大。
3. **并行抓取** — 每条连接都在自己的控制连接上执行 `REST <offset>` + `RETR <path>`，数据连接读取恰好该段的字节数后立即关闭。连接从池中借出、归还，数量受信号量限制，永远不会超过 `-n`。
4. **落盘** — 每个工作线程按绝对偏移写入目标文件（Unix 用 `write_all_at`，Windows 用 `seek_write`），互不干扰，因此不需要共享游标。

**为什么每次提前关闭数据连接是安全的**：段读满后主动关闭会让服务器回 `426`，而服务器对这件事的处理并不统一（有的回 `426`+`226`，有的只回 `426`，有的什么都不回）。客户端不等这个判决，而是直接发一个 `NOOP`：FTP 回复在一条连接上是有序的，排在 `NOOP` 的 `200` 之前的都是被放弃传输的残留回复，读掉即可。这样控制连接总能重新对齐并被复用，也不会为"等一个可能永远不来的回复"付出超时代价。

## 断点续传

断点信息写在目标文件旁边：`<目标文件>.ftpseg-state.json`。

- 传输过程中每 2 秒原子落盘一次（写临时文件再 rename），所以强杀也不会留下截断的状态文件。
- 每段记录 `start`/`len`/`done`，续传时从 `start + done` 继续，因此是**段内粒度**的续传，最多丢 2 秒进度。
- 续传前会校验：远端路径、文件大小、`MDTM` 都一致才复用；不一致就重新开始并给出提示。
- 成功后自动删除状态文件（`--keep-state` 可保留）。
- 目标文件已完整存在且没有状态文件时，会直接报告 "already complete"，用 `--force` 可强制重下。
- 若服务器不支持 `REST`，则退化为单连接下载，此时断点续传不适用（会删掉无意义的状态文件）。

## 字符集与账号密码

FTP 诞生于 Unicode 之前，网线上跑的不一定是 UTF-8。这一节说明客户端怎么决定用哪种编码，以及账号密码从哪来。

### 字符集

字符集只作用于**路径**与**账号密码**。协议动词、应答码、数字参数一律是 ASCII，在支持的所有字符集里编码结果都一样。

`--encoding auto`（默认）的判定顺序：

1. **服务器在 `FEAT` 应答里通告 `UTF8`** → 用 UTF-8，并额外发一条 `OPTS UTF8 ON`（有些服务器要先被明确要求才接受多字节文件名）。
2. 否则 → 用**本机 ANSI 代码页**。这正是图形化 FTP 客户端 "auto" 的做法：中文 Windows 的代码页是 936，于是把"什么都没说"的服务器当作 GBK 处理；macOS/Linux 终端本就是 UTF-8，没有历史包袱，直接用 UTF-8。

代码页会自动映射到对应编码：936→GBK、950→Big5、932→Shift_JIS、949→EUC-KR、54936→GB18030、874/1250–1258→对应 Windows 代码页、65001→UTF-8，其余回落到 UTF-8。

想指定服务器实际用的字符集，直接写名字：

```bash
ftpseg get ftp://host/中文目录/中文文件.bin --encoding gbk
ftpseg ls ftp://host/ --encoding big5
ftpseg get ftp://host/f.bin --encoding shift_jis
ftpseg get ftp://host/f.bin --encoding cp936      # 代码页写法也行
```

可用的名字：`auto`、`utf-8`、`gbk`、`gb18030`、`big5`、`shift_jis`、`euc-kr`、`euc-jp`、`latin-1`、`windows-1251`…以及 `cp936`／`windows-936` 这类代码页写法。名字拼错会在**建立连接之前**就报错。

两个方向的转换都有讲究：

- **发出**：路径和凭据按选定字符集编码。若某个字符该字符集根本表达不了（比如 `--encoding latin-1` 遇到中文），客户端**直接报错**而不是替换成 `?`——后者只会换来一个让人摸不着头脑的 `550 File not found`。
- **收到**：`MLSD`/`LIST` 的目录列表按同一字符集解码，所以 `ls` 能显示正确的中文名，而不是一堆替换字符。

`ftpseg stat` 与 `--dry-run` 会打印实际选中的字符集及其原因：

```
charset     : GBK (system code page 936)
charset     : UTF-8 (server advertises UTF8)
charset     : GBK (--encoding)
```

### 账号密码

四种来源，优先级从高到低：

| 来源 | 说明 |
| --- | --- |
| `--password` / `--user` | 命令行参数 |
| `FTPSEG_PASSWORD` / `FTPSEG_USER` | 环境变量（推荐，不会留在 shell 历史里） |
| URL 里的 `user:password@` | `ftp://alice:s3cret@host/…` |
| 交互式询问 | 有用户名列但没给密码、且 stdin 是终端时提示输入；`--no-prompt` 可关掉，改为直接报错 |
| 匿名 | 既没给用户名也没给密码时用 `anonymous` / `anonymous@` |

命令行参数与 URL 同时给出时，命令行参数优先。

**URL 里的特殊字符**：authority 到**最后一个** `@` 结束，所以密码里的 `@` 和 `:` 可以直接写：

```bash
ftpseg get 'ftp://alice:p@ss:w0rd@example.com/f.bin'   # 密码是 p@ss:w0rd
```

但 `/`、`#`、`?` 会被当作路径或片段的分隔符，必须百分号转义：

```bash
ftpseg get 'ftp://alice:p%2Fss%23x@example.com/f.bin'  # 密码是 p/ss#x
```

忘了转义也不会连错地方：`ftp://alice:p/ss@example.com/f.bin` 会把 `p` 当成主机、`/ss@…` 当成端口而**直接被拒绝**并报 URL 错误（比悄悄连到别处安全）。含这些字符时更推荐用 `--password`。

**非 ASCII 账号密码**同样受字符集控制：URL 里的中文按 UTF-8 百分号转义书写，客户端再转换到服务器字符集发出。也就是说 `--user 用户 --password 密码 --encoding gbk` 会按 GBK 发送这两个字段。反过来，如果字符集选错，服务器解出的就是乱码——表现为 `530 Login incorrect`，此时换 `--encoding` 即可。

## 退出码

| 码 | 含义 |
| --- | --- |
| `0` | 成功 |
| `1` | 失败（网络、协议、认证、磁盘等） |
| `130` | 被 Ctrl-C 中断；已保留断点，重跑同一命令即可续传 |

## 已知限制

- 只支持**被动模式**（`EPSV`/`PASV`）。主动模式在 NAT 与防火墙后基本不可用，现代服务器也默认建议被动模式。
- FTPS 数据连接的 TLS 会话恢复由操作系统 TLS 栈（Windows SChannel / macOS Security.framework）决定。少数强制要求复用控制会话的服务器可能拒绝数据连接，本客户端不做手工会话复用。
- 服务器不支持 `REST`、或无法得知文件大小时，只能单连接顺序下载。
- 字符集是**整条连接统一**的，不能按文件或按目录切换；遇到一个目录用 GBK、另一个用 UTF-8 的服务器，只能分两次调用。
- 只有在服务器自己通告 `UTF8` 时才假定 UTF-8；服务器若既不支持 `FEAT`、又实际使用 UTF-8（罕见），需要显式 `--encoding utf-8`。
- 没有实现 `ABOR`、断点上传、`MLST`。目录递归下载由 GUI 队列支持，CLI `get` 仍仅支持单文件。

## 测试

```sh
cargo test          # 单元测试 + 集成测试
cargo clippy --all-targets
cargo fmt --all -- --check
```

集成测试自带一个**进程内的 FTP 服务器**（`tests/common/mod.rs`），因此不依赖任何外部服务，并且可以精确断言：

- 文件字节完全一致、分段区间无缝且恰好覆盖整个文件；
- 确实开了多条连接（观测服务器端并发传输峰值）；
- 确实用到了 `REST`（记录每个 `REST` 偏移）；
- 中断后重跑会续传而不是重下；
- 服务器缺少 `REST`/`SIZE`/`EPSV`/`MLSD` 时的降级路径；
- 三种"被放弃传输"的回复形态（`426`+`226`、仅 `426`、完全静默）下连接仍可复用、文件仍正确；
- **字符集**：模拟一台只讲 GBK 的老服务器，验证中文文件名可下载、目录列表解码正确、中文账号密码按 GBK 发送（字符集选错时登录确实被拒），以及目标字符集表达不了的字符会明确报错。

## 项目结构

```
src/
  main.rs        命令行入口、子命令实现（含 gui 启动命令）
  gui.rs         本机 GUI HTTP 服务、站点与任务队列
  gui_launcher.rs 双击启动 GUI、打开浏览器（Windows 无控制台）
  gui.html       无外部依赖的跨平台浏览器界面
  cli.rs         clap 参数定义与目标解析（URL / 裸路径）
  client.rs      FTP 控制连接：登录、FEAT/SIZE/MDTM/MLSD、REST+RETR、FTPS 升级
  encoding.rs    字符集协商：auto 判定、代码页映射、路径/凭据编码、列表解码
  protocol/      回复解析、PASV/EPSV 地址解析、MLSD/LIST 列表解析
  transport.rs   TCP/TLS 统一流 + 控制通道行读写
  pool.rs        登录连接池（信号量限流 + 空闲复用）
  plan.rs        分段计划与续传计划校验
  state.rs       断点状态文件（原子读写）
  download.rs    分段下载引擎：工作队列、重试、限速、进度、单连接降级
  progress.rs    indicatif 进度显示
  ratelimit.rs   全局令牌桶限速
  fsutil.rs      跨平台定位写入、预分配、大小解析
  paths.rs       远端路径工具
tests/
  common/mod.rs  进程内测试 FTP 服务器
  integration.rs 端到端测试
scripts/
  install-toolchain.ps1  一次性安装工作区工具链（Windows）
  fetch-mingw.py         下载并解包便携版 MinGW-w64
  env.ps1                工作区工具链环境（Windows）
  cargo.ps1              带环境的 cargo 包装（Windows）
  env.sh / cargo.sh      同上（macOS/Linux）
```

## 许可

MIT
