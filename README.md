# MyWhoosh to iGPSPORT

- 将 MyWhoosh 运动记录自动同步到 iGPSPORT 中国站（`app.igpsport.cn`）的 Rust 命令行工具。
- 本程序会自动修复iGPSPORT网站手动上传MyWhoosh导出fit文件时的时间错误及补全 NP/IF/TSS 数据。
- 原生单文件程序，运行时无需外部依赖。

## 快速开始

Linux 使用 `mywhoosh-to-igpsport`，Windows 使用 `mywhoosh-to-igpsport.exe`。下载对应系统的可执行文件后，在终端中运行。

Linux：

```bash
chmod +x mywhoosh-to-igpsport
./mywhoosh-to-igpsport once --dry-run
./mywhoosh-to-igpsport
```

Windows PowerShell：

```powershell
.\mywhoosh-to-igpsport.exe once --dry-run
.\mywhoosh-to-igpsport.exe
```

首次需要登录时会提示输入账号密码，不保存密码。预演不会上传文件；确认结果后运行不带命令的程序，即每隔 15 分钟检查新活动，按 `Ctrl+C` 停止。

## 构建

需要 Rust stable、系统 C 编译器及链接工具。Linux/macOS 可使用 Bash、curl 和 sha256sum（或 shasum）获取官网签名模块：

```bash
bash scripts/fetch-wasm.sh
cargo build --release --locked
./target/release/mywhoosh-to-igpsport -h
```

签名模块是第三方官网资源，不随源码仓库分发；来源及固定 SHA-256 见 [assets/provenance.json](assets/provenance.json)。Windows 可从该文件记录的地址下载，校验 SHA-256 后放到 `assets/edge_core.wasm`，再执行 `cargo build --release --locked`。

构建后可单独运行 `target/release/mywhoosh-to-igpsport`（Windows 为 `mywhoosh-to-igpsport.exe`）。Linux GNU 版本依赖系统 C 运行库。Windows、macOS 需分别构建目标平台版本。

### Linux 交叉编译 Windows x64

在 Ubuntu/Debian 安装 MinGW 后，可生成 Windows x64 单文件程序：

```bash
sudo apt-get install gcc-mingw-w64-x86-64-posix
rustup target add x86_64-pc-windows-gnu
bash scripts/fetch-wasm.sh
cargo build --release --locked --target x86_64-pc-windows-gnu
```

产物为 `target/x86_64-pc-windows-gnu/release/mywhoosh-to-igpsport.exe`。在 Windows PowerShell 中运行：

```powershell
.\mywhoosh-to-igpsport.exe -h
.\mywhoosh-to-igpsport.exe check
.\mywhoosh-to-igpsport.exe once --dry-run
```

## 使用

不指定命令时默认使用 `run`，持续同步，每轮完成后等待 900 秒；`Ctrl+C` 停止。只执行一轮请显式指定 `once`。

```bash
# 默认持续同步
./mywhoosh-to-igpsport

# 检查连接和 FTP，不上传
./mywhoosh-to-igpsport check

# 预演一轮同步
./mywhoosh-to-igpsport once --dry-run

# 同步一条需要上传的记录
./mywhoosh-to-igpsport once --limit 1

# 忽略本地去重，仍检查目标端重复
./mywhoosh-to-igpsport once --resync --limit 1

# 每轮完成后等待 15 分钟继续同步
./mywhoosh-to-igpsport run --interval 900

# 查看本地同步状态
./mywhoosh-to-igpsport status

# 完整帮助
./mywhoosh-to-igpsport -h
```

## AI 运动分析

点评采用类似 Athlete Intelligence 的叙述：先点出本次亮点，再结合近期表现、课程意图和训练连续性给出积极且有依据的评价，不固定罗列数据或布置下一次训练。

每轮读取 MyWhoosh 列表时缓存活动历史，AI 分析时自动附加本次活动之前的历史统计：近 21 天最高速度、近 30 天平均速度和距离、已记录的连续训练天数。速度统一按距离/记录总时长计算，含暂停，可能与平台显示口径不同；虚拟路线和跟骑会影响速度，不直接等同体能进步。

功率参考 5 秒、30 秒、1 分钟、5 分钟、8 分钟、20 分钟和30分钟最佳持续输出，计算在已缓存 MyWhoosh 原始 FIT 中的排名及严格新高；并列不算刷新纪录，无历史样本不称个人纪录。为保持同步效率，不额外下载历史 FIT，已被目标去重跳过且本地没有 FIT 的活动只参与列表统计。历史缓存按 MyWhoosh 账号隔离，功率缓存沿用当前同步目标的数据目录；使用 `--since`、清理缓存或切换目标会影响覆盖范围，因此不代表完整生涯纪录或所有平台的训练连续性。历史上传只比较运动开始之前的活动，不使用未来记录。首次仅预演时不会写入历史缓存，历史比较依赖此前普通运行积累的缓存。

不复现 Strava 专有的相对负荷度，也不会把 TSS 当作相对负荷度；未知阈值心率时不推测心率区间。历史不足时仅评价本次实际数据。功能无需增加参数，仍使用 `--deepseek-key`。

AI 输入包含 MyWhoosh 原始活动标题（包括其中的训练课程名称），用于结合实际运动数据理解课程意图；不会用上传后统一的“室内骑行”名称替代原始标题。课程名不能代替完整课表，模型不会仅凭标题推测目标功率、组数或完成率。

添加 `--deepseek-key` 可启用 DeepSeek，将本次运动分析写入新上传活动的“说明”：

```bash
./mywhoosh-to-igpsport once --deepseek-key '你的 API Key'
# 持续同步并分析新活动
./mywhoosh-to-igpsport --deepseek-key '你的 API Key'
# 预览待上传活动的分析，不上传或修改 iGPSPORT
./mywhoosh-to-igpsport once --dry-run --deepseek-key '你的 API Key'
```

使用 `deepseek-chat` 模型，基于 FIT 的功率、心率、踏频及 FTP/NP/IF/TSS 等指标分析训练刺激、输出结构和下次训练建议，避免复述数据清单与通用建议。全程逐秒功率用于计算七个 FTP 功率区间的累计时间、5 秒至 20 分钟最佳持续功率和功率变异指数。不超过 1 小时的活动直接发送完整逐秒功率、心率、踏频；更长活动发送覆盖全程的 30 秒统计块，包含有效样本数、均值、最小值和最大值，以控制请求长度。不发送账号和 GPS 坐标，缺失指标不推测。

分析为中文纯文本，目标 120–220 字，最多 500 个 UTF-16 单元，导入后写入说明并读取核验。当前 FTP 可能与历史运动时不同，模型按此限制解释数据，不固定输出免责声明。没有已知心率阈值时不推测心率分区，没有合适稳定片段时不判断心率漂移。

不提供 Key 时不调用 AI。已存在的重复活动仍然跳过，不补写旧记录。AI 失败会停止该条上传并保留待处理状态；下次携带 Key 可重试。已生成的分析随任务保存在本地，用于中断恢复，说明写入失败后会继续核对并补写，不重复上传 FIT。预演也会实际调用 DeepSeek 并产生 API 费用，但仅分析需要上传的候选活动。

API Key 只通过 CLI 传入，不写入配置或登录凭证文件；与 CLI 密码一样，可能出现在进程参数及终端历史中。网络超时沿用 `--timeout`，分析较慢时可设置 `--timeout 180`。

启用 AI 后，每轮结束显示 DeepSeek 返回的实际输入、输出及总 Token 消耗。缓存复用或没有调用时为 0；请求失败或未返回完整用量时单独提示未统计的请求数。即使生成内容校验失败，接口已返回的用量也会计入。提示词及数据格式升级后，尚未提交的新活动会重新生成过期分析；已上传活动仍按去重规则跳过，不自动改写说明。

## 登录与数据

所有参数通过 CLI 配置，未提供账号密码时优先复用登录凭证；缺失或失效时提示输入。需要无交互运行时通过参数提供账号密码：

```bash
./mywhoosh-to-igpsport once \
  --myid '邮箱' --mypw '密码' \
  --igpid '手机号或邮箱' --igppw '密码'
```

优先使用已保存的登录凭证；凭证缺失或失效时，才使用提供的账号密码登录，缺少的部分提示输入。`login` 命令强制重新登录两端，例如 `./mywhoosh-to-igpsport login --myid '邮箱' --mypw '密码' --igpid '手机号或邮箱' --igppw '密码'`。MyWhoosh 登录检测到其他设备登录冲突时，会强制登出其他设备后重试。

本地不保存账号密码，仅保存令牌、设备 ID、同步状态及 FIT 缓存。CLI 密码可能进入 shell 历史及进程参数，交互输入可避免这一点。Unix 登录凭证与缓存文件权限为 0600，目录权限为 0700。

默认数据目录保存登录凭证和缓存。Linux 通常为 `~/.local/share/mywhoosh-sync`，可用 `--data-dir /path` 指定。目录包含：

- `mywhoosh-session.json`、`igpsport-session.json`：登录凭证。
- `sync.sqlite3`：活动列表缓存、同步状态及恢复进度。
- `fit/`：原始 FIT 和上传副本。

## 同步规则

- 普通模式先读取 MyWhoosh 列表，与 SQLite 中按 MyWhoosh 账号哈希隔离的活动 ID 缓存比较；没有新增或待处理活动时，本轮不连接 iGPSPORT，不读取远端列表或下载 FIT。首次运行会检查现有活动，确认已同步或远端已存在后缓存为已完成。失败、待核对及因 `--limit` 尚未处理的活动保留待处理状态，下轮继续；`--resync` 跳过缓存比较，执行完整远端检查。缓存只按新增活动 ID 判断，已有活动的修改或目标端删除需用 `--resync` 重新检查。切换 iGPSPORT 账号也应使用 `--resync`。
- 下载并保留服务器原始 FIT，仅修改上传副本的汇总字段和 CRC，保留原始 WGS84 坐标、逐秒采样及 developer payload。
- 修正时间、最大速度，从 MyWhoosh 当前个人资料获取 FTP，补算 NP/IF/TSS。功率计算要求连续每秒采样、无暂停、至少 30 秒。
- 默认将导入后的活动命名为“室内骑行”，可用 `--activity-title` 修改。上传后读取详情核验时间、速度及功率指标。
- 每轮先用 MyWhoosh 列表中的开始时间、总时长、距离、总爬升，与 iGPSPORT 对应日期范围内的列表直接比较；唯一匹配则跳过且不读取或下载 FIT。未匹配、匹配不唯一或 MyWhoosh 列表字段不完整时，才读取/下载原始 FIT，并以 FIT 汇总值再次进行远端去重后决定上传。iGPSPORT 查询起点为本轮 MyWhoosh 活动列表最早日期，终点为今天（若活动日期晚于今天，则延伸至最晚活动日期），包含首尾日期；通过 `beginTime`、`endTime` 筛选，每页请求 20 条，按返回总条数读完范围内的所有分页。FIT 实际日期超出列表范围时会补查。`--resync` 同样执行远端去重。
- MyWhoosh 活动列表使用网页版接口 `POST https://service14.mywhoosh.com/v2/rider/profile/activities`，沿用现有登录凭证，请求参数为 `type=0`、`page` 和 `sortDate=DESC`。列表使用 `startDatetime`、`rideDuration`（如 `01:00:58.000`）、`distance`（公里，保留两位小数）、`elevation`（米）。列表初筛及下载后的 FIT 校验均要求配置时区下日期相同、记录时长差 ≤2 秒、距离差 ≤10 米、总爬升差 ≤2 米。FIT 使用 session 的开始时间、total_elapsed_time、total_distance、total_ascent；目标列表使用 startTime、recordTime、rideDistance、totalAscent。`--max-pages` 限制两个平台的列表分页；iGPSPORT 范围未读完、分页重复或总条数变化时，本轮停止上传。本轮新提交或预演的活动也参与精确去重。`--limit` 限制后续处理数量。
- 普通同步同时检查本地列表缓存、任务状态和文件哈希。`--resync` 忽略本地列表缓存、任务状态及哈希，但保留远端四项去重，不修改目标已有记录；使用前请等待上次导入处理结束。
- 提交结果不确定时保存进度，普通模式先核对，不盲目重传。`--dry-run` 不上传、不修改同步任务，但仍会登录、读取列表及缓存原始 FIT。
- `--since YYYY-MM-DD` 筛选活动日期，`--activity-id` 限定单条，`--limit` 限制每轮上传候选数量；远端重复跳过不占额度。

## 开发

```bash
bash scripts/fetch-wasm.sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

测试使用合成数据，不需要账号密码、真实运动记录。

## 许可

Rust 源码采用 [MIT](LICENSE) 许可，第三方依赖及官网签名模块见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。本工具依赖平台客户端接口，官网或 App 更新可能导致功能失效。
